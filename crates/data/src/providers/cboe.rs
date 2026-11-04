//! CBOE delayed quotes (free, ~15 minute delay, no key).
//!
//! * Chain:  `https://cdn.cboe.com/api/global/delayed_quotes/options/{SYM}.json`
//! * Quote:  `https://cdn.cboe.com/api/global/delayed_quotes/quotes/{SYM}.json`
//!
//! Index symbols take an underscore prefix (`_SPX`, `_NDX`, `_XSP`, `_RUT`, `_VIX`);
//! stocks and ETFs are plain (`SPY`, `QQQ`). Each contract row carries the CBOE
//! computed IV (decimal) and greeks (delta, gamma, theta per day, vega per 1 vol point).

use super::{clean_symbol, finite, is_index_symbol, positive, style_for};
use crate::http::{send_text, LARGE_TIMEOUT};
use anyhow::{anyhow, Context, Result};
use chrono::NaiveDateTime;
use optlab_core::chain::parse_occ_symbol;
use optlab_core::{OptionChain, OptionQuote};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};

pub const SOURCE: &str = "cboe";
const BASE: &str = "https://cdn.cboe.com/api/global/delayed_quotes";

/// CBOE path symbol: `_SPX` for index roots, plain ticker otherwise.
pub fn cboe_symbol(sym: &str) -> String {
    let c = clean_symbol(sym);
    // Weekly/PM roots are listed under their parent index.
    let parent = match c.as_str() {
        "SPXW" | "SPXPM" => "SPX",
        "NDXP" => "NDX",
        "RUTW" => "RUT",
        "VIXW" => "VIX",
        other => other,
    };
    if is_index_symbol(parent) {
        format!("_{parent}")
    } else {
        parent.to_string()
    }
}

pub fn chain_url(sym: &str) -> String {
    format!("{BASE}/options/{}.json", cboe_symbol(sym))
}

pub fn quote_url(sym: &str) -> String {
    format!("{BASE}/quotes/{}.json", cboe_symbol(sym))
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    timestamp: Option<String>,
    data: T,
    #[allow(dead_code)]
    symbol: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChainData {
    symbol: Option<String>,
    current_price: Option<f64>,
    close: Option<f64>,
    prev_day_close: Option<f64>,
    #[serde(default)]
    options: Vec<CboeOption>,
}

#[derive(Debug, Deserialize)]
struct CboeOption {
    option: String,
    bid: Option<f64>,
    ask: Option<f64>,
    iv: Option<f64>,
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
    volume: Option<f64>,
    open_interest: Option<f64>,
    last_trade_price: Option<f64>,
}

/// Underlying quote from the CBOE quote endpoint (`iv30` in vol points, e.g. 24.9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CboeQuote {
    pub symbol: String,
    pub security_type: Option<String>,
    pub price: f64,
    pub prev_close: Option<f64>,
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    /// 30-day implied volatility in percent points (24.9 = 24.9%). For `_VIX` it equals the index.
    pub iv30: Option<f64>,
    pub as_of: Option<NaiveDateTime>,
    pub last_trade_time: Option<NaiveDateTime>,
}

#[derive(Debug, Deserialize)]
struct QuoteData {
    symbol: Option<String>,
    security_type: Option<String>,
    current_price: Option<f64>,
    close: Option<f64>,
    prev_day_close: Option<f64>,
    open: Option<f64>,
    high: Option<f64>,
    low: Option<f64>,
    iv30: Option<f64>,
    last_trade_time: Option<String>,
}

fn parse_ts(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim();
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
        .ok()
}

/// Parse a CBOE delayed-quotes options payload into an [`OptionChain`].
///
/// `requested` is the symbol the user asked for (used for the underlying name
/// and the exercise style). `as_of` comes from the payload `timestamp`.
pub fn parse_chain(json: &str, requested: &str) -> Result<OptionChain> {
    let env: Envelope<ChainData> = serde_json::from_str(json).context("CBOE: unexpected chain JSON shape")?;
    let d = env.data;
    let spot = positive(d.current_price)
        .or(positive(d.close))
        .or(positive(d.prev_day_close))
        .ok_or_else(|| anyhow!("CBOE: chain for {requested} has no underlying price"))?;
    let as_of = env
        .timestamp
        .as_deref()
        .and_then(parse_ts)
        .unwrap_or_else(crate::util::now_new_york);
    let underlying = {
        let r = clean_symbol(requested);
        if r.is_empty() {
            clean_symbol(d.symbol.as_deref().unwrap_or(""))
        } else {
            r
        }
    };
    let mut quotes = Vec::with_capacity(d.options.len());
    for o in d.options {
        let Some((_root, expiry, kind, strike)) = parse_occ_symbol(&o.option) else { continue };
        let iv = positive(o.iv);
        // CBOE reports 0 greeks when it could not compute an IV; treat as missing.
        let has_model = iv.is_some();
        let g = |v: Option<f64>| if has_model { finite(v) } else { None };
        quotes.push(OptionQuote {
            contract: o.option.trim().to_string(),
            expiry,
            strike,
            kind,
            bid: o.bid.unwrap_or(0.0).max(0.0),
            ask: o.ask.unwrap_or(0.0).max(0.0),
            last: o.last_trade_price.unwrap_or(0.0).max(0.0),
            volume: o.volume.unwrap_or(0.0),
            open_interest: o.open_interest.unwrap_or(0.0),
            iv,
            delta: g(o.delta),
            gamma: g(o.gamma),
            theta: g(o.theta),
            vega: g(o.vega),
        });
    }
    if quotes.is_empty() {
        return Err(anyhow!("CBOE: chain for {requested} contained no parsable contracts"));
    }
    Ok(OptionChain {
        style: style_for(&underlying),
        underlying,
        spot,
        as_of,
        source: SOURCE.to_string(),
        quotes,
    })
}

/// Parse a CBOE quote payload (also works on the chain payload, which embeds the quote fields).
pub fn parse_quote(json: &str) -> Result<CboeQuote> {
    let env: Envelope<QuoteData> = serde_json::from_str(json).context("CBOE: unexpected quote JSON shape")?;
    let d = env.data;
    let price = positive(d.current_price)
        .or(positive(d.close))
        .or(positive(d.prev_day_close))
        .ok_or_else(|| anyhow!("CBOE: quote has no price"))?;
    Ok(CboeQuote {
        symbol: clean_symbol(d.symbol.as_deref().unwrap_or("")),
        security_type: d.security_type,
        price,
        prev_close: positive(d.prev_day_close),
        open: positive(d.open),
        high: positive(d.high),
        low: positive(d.low),
        iv30: positive(d.iv30),
        as_of: env.timestamp.as_deref().and_then(parse_ts),
        last_trade_time: d.last_trade_time.as_deref().and_then(parse_ts),
    })
}

fn get(client: &Client, url: &str, what: &str, large: bool) -> Result<String> {
    let mut req = client
        .get(url)
        .header("Accept", "application/json")
        .header("Referer", "https://www.cboe.com/");
    if large {
        req = req.timeout(LARGE_TIMEOUT);
    }
    send_text(req, what)
}

/// Download and parse the full delayed chain (all expiries) for `sym`.
pub fn fetch_chain(client: &Client, sym: &str) -> Result<OptionChain> {
    let body = get(client, &chain_url(sym), &format!("CBOE chain {}", cboe_symbol(sym)), true)?;
    parse_chain(&body, sym)
}

/// Download the delayed underlying quote (price, IV30).
pub fn fetch_quote(client: &Client, sym: &str) -> Result<CboeQuote> {
    let body = get(client, &quote_url(sym), &format!("CBOE quote {}", cboe_symbol(sym)), false)?;
    parse_quote(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls() {
        assert_eq!(chain_url("SPX"), "https://cdn.cboe.com/api/global/delayed_quotes/options/_SPX.json");
        assert_eq!(chain_url("^ndx"), "https://cdn.cboe.com/api/global/delayed_quotes/options/_NDX.json");
        assert_eq!(chain_url("SPXW"), "https://cdn.cboe.com/api/global/delayed_quotes/options/_SPX.json");
        assert_eq!(chain_url("qqq"), "https://cdn.cboe.com/api/global/delayed_quotes/options/QQQ.json");
        assert_eq!(quote_url("VIX"), "https://cdn.cboe.com/api/global/delayed_quotes/quotes/_VIX.json");
    }
}
