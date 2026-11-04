//! Tradier brokerage market data (requires an access token; sandbox tokens are free
//! but delayed). Greeks/IV come from ORATS and are refreshed about once per hour.
//!
//! * `GET {base}/markets/options/expirations?symbol=SPY&includeAllRoots=true`
//! * `GET {base}/markets/options/chains?symbol=SPY&expiration=2026-12-18&greeks=true`
//! * `GET {base}/markets/quotes?symbols=SPY`
//!
//! Base: `https://api.tradier.com/v1` (production) or `https://sandbox.tradier.com/v1`.
//! Tradier collapses single-element arrays into objects and empty results into
//! `null`, so the parsers accept both shapes.

use super::{clean_symbol, finite, positive, style_for};
use crate::http::send_text;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use optlab_core::chain::parse_occ_symbol;
use optlab_core::{OptionChain, OptionKind, OptionQuote};
use reqwest::blocking::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;

pub const SOURCE: &str = "tradier";
pub const PRODUCTION_BASE: &str = "https://api.tradier.com/v1";
pub const SANDBOX_BASE: &str = "https://sandbox.tradier.com/v1";

/// Accept `T`, `[T]` or `null` and return a Vec.
fn one_or_many<T: DeserializeOwned>(v: Option<&Value>) -> Result<Vec<T>> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(|x| serde_json::from_value(x.clone()).map_err(Into::into)).collect(),
        Some(other) => Ok(vec![serde_json::from_value(other.clone())?]),
    }
}

#[derive(Debug, Deserialize)]
struct TOption {
    symbol: String,
    strike: Option<f64>,
    option_type: Option<String>,
    expiration_date: Option<String>,
    bid: Option<f64>,
    ask: Option<f64>,
    last: Option<f64>,
    volume: Option<f64>,
    open_interest: Option<f64>,
    greeks: Option<TGreeks>,
}

#[derive(Debug, Deserialize)]
struct TGreeks {
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
    mid_iv: Option<f64>,
    smv_vol: Option<f64>,
    updated_at: Option<String>,
}

/// Parse `/markets/options/expirations` → sorted expiry dates.
pub fn parse_expirations(json: &str) -> Result<Vec<NaiveDate>> {
    let v: Value = serde_json::from_str(json).context("Tradier: invalid expirations JSON")?;
    let dates: Vec<String> = one_or_many(v.get("expirations").and_then(|e| e.get("date")))?;
    let mut out: Vec<NaiveDate> = dates
        .iter()
        .filter_map(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// Parse `/markets/options/chains` → quotes. Also returns the latest greeks `updated_at`.
pub fn parse_chain(json: &str) -> Result<(Vec<OptionQuote>, Option<NaiveDateTime>)> {
    let v: Value = serde_json::from_str(json).context("Tradier: invalid chain JSON")?;
    let opts: Vec<TOption> = one_or_many(v.get("options").and_then(|o| o.get("option")))?;
    let mut latest: Option<NaiveDateTime> = None;
    let mut quotes = Vec::with_capacity(opts.len());
    for o in opts {
        let occ = parse_occ_symbol(&o.symbol);
        let kind = match o.option_type.as_deref().map(|s| s.to_ascii_lowercase()) {
            Some(ref s) if s == "call" => OptionKind::Call,
            Some(ref s) if s == "put" => OptionKind::Put,
            _ => match &occ {
                Some(p) => p.2,
                None => continue,
            },
        };
        let expiry = o
            .expiration_date
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .or(occ.as_ref().map(|p| p.1));
        let (Some(expiry), Some(strike)) = (expiry, o.strike.or(occ.as_ref().map(|p| p.3))) else { continue };
        let g = o.greeks.as_ref();
        if let Some(t) = g
            .and_then(|g| g.updated_at.as_deref())
            .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
        {
            latest = Some(latest.map_or(t, |l| l.max(t)));
        }
        quotes.push(OptionQuote {
            contract: o.symbol.clone(),
            expiry,
            strike,
            kind,
            bid: o.bid.unwrap_or(0.0).max(0.0),
            ask: o.ask.unwrap_or(0.0).max(0.0),
            last: o.last.unwrap_or(0.0).max(0.0),
            volume: o.volume.unwrap_or(0.0),
            open_interest: o.open_interest.unwrap_or(0.0),
            iv: positive(g.and_then(|g| g.mid_iv)).or(positive(g.and_then(|g| g.smv_vol))),
            delta: finite(g.and_then(|g| g.delta)),
            gamma: finite(g.and_then(|g| g.gamma)),
            theta: finite(g.and_then(|g| g.theta)),
            vega: finite(g.and_then(|g| g.vega)),
        });
    }
    Ok((quotes, latest))
}

/// Underlying quote from `/markets/quotes`.
#[derive(Debug, Clone, PartialEq)]
pub struct TradierQuote {
    pub symbol: String,
    pub price: f64,
    pub time: Option<NaiveDateTime>,
}

#[derive(Debug, Deserialize)]
struct TQuote {
    symbol: String,
    last: Option<f64>,
    bid: Option<f64>,
    ask: Option<f64>,
    prevclose: Option<f64>,
    close: Option<f64>,
    trade_date: Option<i64>,
}

/// Parse `/markets/quotes` → first quote.
pub fn parse_quotes(json: &str) -> Result<Vec<TradierQuote>> {
    let v: Value = serde_json::from_str(json).context("Tradier: invalid quotes JSON")?;
    let qs: Vec<TQuote> = one_or_many(v.get("quotes").and_then(|q| q.get("quote")))?;
    Ok(qs
        .into_iter()
        .filter_map(|q| {
            let mid = match (positive(q.bid), positive(q.ask)) {
                (Some(b), Some(a)) if a >= b => Some(0.5 * (a + b)),
                _ => None,
            };
            let price = positive(q.last).or(mid).or(positive(q.close)).or(positive(q.prevclose))?;
            Some(TradierQuote {
                symbol: q.symbol,
                price,
                time: q
                    .trade_date
                    .and_then(DateTime::from_timestamp_millis)
                    .map(|d| crate::util::unix_to_new_york(d.timestamp())),
            })
        })
        .collect())
}

/// Blocking Tradier client.
pub struct TradierClient {
    client: Client,
    token: String,
    base: String,
}

impl TradierClient {
    pub fn new(client: Client, token: impl Into<String>, sandbox: bool) -> Self {
        Self {
            client,
            token: token.into(),
            base: if sandbox { SANDBOX_BASE } else { PRODUCTION_BASE }.to_string(),
        }
    }

    fn get(&self, path: &str, query: &[(&str, &str)], what: &str) -> Result<String> {
        if self.token.trim().is_empty() {
            return Err(anyhow!("Tradier: no access token configured (set TRADIER_TOKEN or config.toml tradier_token)"));
        }
        let req = self
            .client
            .get(format!("{}{}", self.base, path))
            .query(query)
            .bearer_auth(self.token.trim())
            .header("Accept", "application/json");
        send_text(req, what)
    }

    pub fn expirations(&self, sym: &str) -> Result<Vec<NaiveDate>> {
        let s = clean_symbol(sym);
        let body = self.get(
            "/markets/options/expirations",
            &[("symbol", &s), ("includeAllRoots", "true")],
            &format!("Tradier expirations {s}"),
        )?;
        parse_expirations(&body)
    }

    pub fn chain_for_expiry(&self, sym: &str, expiry: NaiveDate) -> Result<Vec<OptionQuote>> {
        let s = clean_symbol(sym);
        let e = expiry.format("%Y-%m-%d").to_string();
        let body = self.get(
            "/markets/options/chains",
            &[("symbol", &s), ("expiration", &e), ("greeks", "true")],
            &format!("Tradier chain {s} {e}"),
        )?;
        Ok(parse_chain(&body)?.0)
    }

    pub fn quote(&self, sym: &str) -> Result<TradierQuote> {
        let s = clean_symbol(sym);
        let body = self.get("/markets/quotes", &[("symbols", &s), ("greeks", "false")], &format!("Tradier quote {s}"))?;
        parse_quotes(&body)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Tradier: no quote returned for {s}"))
    }

    /// Full chain over all (or the nearest `max_expiries`) expirations.
    pub fn fetch_chain(&self, sym: &str, max_expiries: Option<usize>) -> Result<OptionChain> {
        let q = self.quote(sym)?;
        let mut exps = self.expirations(sym)?;
        if exps.is_empty() {
            return Err(anyhow!("Tradier: no option expirations for {}", clean_symbol(sym)));
        }
        if let Some(m) = max_expiries {
            exps.truncate(m.max(1));
        }
        let mut quotes = Vec::new();
        for e in exps {
            quotes.extend(self.chain_for_expiry(sym, e)?);
        }
        let underlying = clean_symbol(sym);
        Ok(OptionChain {
            style: style_for(&underlying),
            underlying,
            spot: q.price,
            as_of: q.time.unwrap_or_else(crate::util::now_new_york),
            source: SOURCE.to_string(),
            quotes,
        })
    }
}
