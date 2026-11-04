//! Yahoo Finance (free, unofficial, no key).
//!
//! * Cookie + crumb: `GET https://fc.yahoo.com` (sets cookies, status ignored), then
//!   `GET https://query2.finance.yahoo.com/v1/test/getcrumb` (plain-text crumb).
//! * Options: `/v7/finance/options/{SYM}?date={unix}&crumb=...` — one expiry per call;
//!   the first call lists `expirationDates`.
//! * History: `/v8/finance/chart/{SYM}?period1&period2&interval=1d&events=div,split`.
//!
//! Yahoo supplies IV (decimal) but no greeks; greeks are left `None` for the
//! analysis layer to compute.

use super::{clean_symbol, is_index_symbol, positive, style_for};
use crate::http::send_text;
use crate::util::unix_to_new_york;
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime};
use optlab_core::chain::parse_occ_symbol;
use optlab_core::{Bar, OptionChain, OptionKind, OptionQuote, PriceSeries, ScalarSeries};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::sync::Mutex;

pub const SOURCE: &str = "yahoo";
const Q2: &str = "https://query2.finance.yahoo.com";
const Q1: &str = "https://query1.finance.yahoo.com";

/// Yahoo ticker: `^SPX` / `^NDX` / `^VIX` for indices, `BRK-B` for class shares.
pub fn yahoo_symbol(sym: &str) -> String {
    let t = sym.trim();
    if t.starts_with('^') || t.contains('=') {
        return t.to_ascii_uppercase();
    }
    let c = clean_symbol(t);
    let idx = matches!(c.as_str(), "IRX" | "TNX" | "FVX" | "TYX" | "VXN" | "VIX3M" | "VVIX" | "GSPC" | "NDX" | "SPX");
    if is_index_symbol(&c) || idx {
        format!("^{c}")
    } else {
        c.replace('.', "-")
    }
}

// ---------------------------------------------------------------- options JSON

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OptionsResponse {
    option_chain: OptionChainEnv,
}

#[derive(Debug, Deserialize)]
struct OptionChainEnv {
    #[serde(default)]
    result: Vec<OptionsResult>,
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OptionsResult {
    underlying_symbol: Option<String>,
    #[serde(default)]
    expiration_dates: Vec<i64>,
    quote: Option<YQuote>,
    #[serde(default)]
    options: Vec<YExpiry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YQuote {
    regular_market_price: Option<f64>,
    regular_market_time: Option<i64>,
    regular_market_previous_close: Option<f64>,
    bid: Option<f64>,
    ask: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct YExpiry {
    #[serde(default)]
    calls: Vec<YContract>,
    #[serde(default)]
    puts: Vec<YContract>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YContract {
    contract_symbol: String,
    strike: Option<f64>,
    last_price: Option<f64>,
    volume: Option<f64>,
    open_interest: Option<f64>,
    bid: Option<f64>,
    ask: Option<f64>,
    expiration: Option<i64>,
    implied_volatility: Option<f64>,
}

/// One page of the v7 options endpoint (one expiry's calls and puts).
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsPage {
    pub underlying: String,
    pub spot: f64,
    /// Exchange (New York) time of the underlying quote.
    pub market_time: Option<NaiveDateTime>,
    /// All listed expiries as UNIX timestamps (00:00 UTC of the expiry date).
    pub expiration_dates: Vec<i64>,
    pub quotes: Vec<OptionQuote>,
}

fn utc_date(ts: i64) -> Option<NaiveDate> {
    DateTime::from_timestamp(ts, 0).map(|d| d.date_naive())
}

fn convert(c: &YContract, kind: OptionKind) -> Option<OptionQuote> {
    let parsed = parse_occ_symbol(&c.contract_symbol);
    let expiry = parsed
        .as_ref()
        .map(|p| p.1)
        .or_else(|| c.expiration.and_then(utc_date))?;
    let strike = c.strike.or(parsed.as_ref().map(|p| p.3))?;
    // Yahoo reports ~1e-5 (or tiny values) when it has no IV.
    let iv = c.implied_volatility.filter(|v| v.is_finite() && *v > 0.001);
    Some(OptionQuote {
        contract: c.contract_symbol.clone(),
        expiry,
        strike,
        kind,
        bid: c.bid.unwrap_or(0.0).max(0.0),
        ask: c.ask.unwrap_or(0.0).max(0.0),
        last: c.last_price.unwrap_or(0.0).max(0.0),
        volume: c.volume.unwrap_or(0.0),
        open_interest: c.open_interest.unwrap_or(0.0),
        iv,
        delta: None,
        gamma: None,
        theta: None,
        vega: None,
    })
}

/// Parse one v7 options response body.
pub fn parse_options_page(json: &str) -> Result<OptionsPage> {
    let r: OptionsResponse = serde_json::from_str(json).context("Yahoo: unexpected options JSON shape")?;
    if let Some(e) = r.option_chain.error.filter(|e| !e.is_null()) {
        return Err(anyhow!("Yahoo options error: {e}"));
    }
    let res = r
        .option_chain
        .result
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Yahoo: options response has no result (unknown symbol?)"))?;
    let q = res.quote.as_ref();
    let spot = q
        .and_then(|q| positive(q.regular_market_price))
        .or_else(|| q.and_then(|q| match (positive(q.bid), positive(q.ask)) {
            (Some(b), Some(a)) => Some(0.5 * (a + b)),
            _ => None,
        }))
        .or_else(|| q.and_then(|q| positive(q.regular_market_previous_close)))
        .ok_or_else(|| anyhow!("Yahoo: options response has no underlying price"))?;
    let mut quotes = Vec::new();
    for e in &res.options {
        quotes.extend(e.calls.iter().filter_map(|c| convert(c, OptionKind::Call)));
        quotes.extend(e.puts.iter().filter_map(|c| convert(c, OptionKind::Put)));
    }
    Ok(OptionsPage {
        underlying: res.underlying_symbol.unwrap_or_default(),
        spot,
        market_time: q.and_then(|q| q.regular_market_time).map(unix_to_new_york),
        expiration_dates: res.expiration_dates,
        quotes,
    })
}

/// Merge pages (one per expiry) into a chain.
pub fn chain_from_pages(requested: &str, pages: &[OptionsPage]) -> Result<OptionChain> {
    let first = pages.first().ok_or_else(|| anyhow!("Yahoo: no option pages"))?;
    let mut quotes: Vec<OptionQuote> = pages.iter().flat_map(|p| p.quotes.iter().cloned()).collect();
    quotes.sort_by(|a, b| {
        (a.expiry, a.kind as u8, a.strike)
            .partial_cmp(&(b.expiry, b.kind as u8, b.strike))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    quotes.dedup_by(|a, b| a.contract == b.contract);
    let underlying = clean_symbol(requested);
    Ok(OptionChain {
        style: style_for(&underlying),
        underlying,
        spot: first.spot,
        as_of: first.market_time.unwrap_or_else(crate::util::now_new_york),
        source: SOURCE.to_string(),
        quotes,
    })
}

// ---------------------------------------------------------------- chart JSON

#[derive(Debug, Deserialize)]
struct ChartResponse {
    chart: ChartEnv,
}
#[derive(Debug, Deserialize)]
struct ChartEnv {
    result: Option<Vec<ChartResult>>,
    error: Option<serde_json::Value>,
}
#[derive(Debug, Deserialize)]
struct ChartResult {
    meta: ChartMeta,
    #[serde(default)]
    timestamp: Vec<i64>,
    indicators: Indicators,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChartMeta {
    symbol: Option<String>,
    gmtoffset: Option<i64>,
}
#[derive(Debug, Deserialize)]
struct Indicators {
    #[serde(default)]
    quote: Vec<QuoteInd>,
    #[serde(default)]
    adjclose: Vec<AdjInd>,
}
#[derive(Debug, Deserialize, Default)]
struct QuoteInd {
    #[serde(default)]
    open: Vec<Option<f64>>,
    #[serde(default)]
    high: Vec<Option<f64>>,
    #[serde(default)]
    low: Vec<Option<f64>>,
    #[serde(default)]
    close: Vec<Option<f64>>,
    #[serde(default)]
    volume: Vec<Option<f64>>,
}
#[derive(Debug, Deserialize)]
struct AdjInd {
    #[serde(default)]
    adjclose: Vec<Option<f64>>,
}

/// Parse a v8 chart response (interval=1d) into a daily [`PriceSeries`].
///
/// Dates are the exchange-local date (`timestamp + gmtoffset`). Rows with a
/// null close are skipped; when Yahoo appends a live intraday row with the same
/// date as the last daily row, the later (live) row wins.
pub fn parse_chart(json: &str, symbol: &str) -> Result<PriceSeries> {
    let r: ChartResponse = serde_json::from_str(json).context("Yahoo: unexpected chart JSON shape")?;
    if let Some(e) = r.chart.error.filter(|e| !e.is_null()) {
        return Err(anyhow!("Yahoo chart error: {e}"));
    }
    let res = r
        .chart
        .result
        .and_then(|v| v.into_iter().next())
        .ok_or_else(|| anyhow!("Yahoo: chart response has no result"))?;
    let off = res.meta.gmtoffset.unwrap_or(-5 * 3600);
    let q = res.indicators.quote.into_iter().next().unwrap_or_default();
    let adj = res.indicators.adjclose.into_iter().next().map(|a| a.adjclose).unwrap_or_default();
    let at = |v: &Vec<Option<f64>>, i: usize| v.get(i).copied().flatten().filter(|x| x.is_finite());
    let mut bars: Vec<Bar> = Vec::with_capacity(res.timestamp.len());
    for (i, ts) in res.timestamp.iter().enumerate() {
        let Some(close) = at(&q.close, i).filter(|c| *c > 0.0) else { continue };
        let Some(date) = utc_date(ts + off) else { continue };
        let bar = Bar {
            date,
            open: at(&q.open, i).unwrap_or(close),
            high: at(&q.high, i).unwrap_or(close),
            low: at(&q.low, i).unwrap_or(close),
            close,
            adj_close: at(&adj, i).filter(|a| *a > 0.0).unwrap_or(close),
            volume: at(&q.volume, i).unwrap_or(0.0),
        };
        match bars.last_mut() {
            Some(last) if last.date == date => *last = bar,
            _ => bars.push(bar),
        }
    }
    if bars.is_empty() {
        return Err(anyhow!("Yahoo: chart for {symbol} has no bars"));
    }
    let name = if symbol.is_empty() { res.meta.symbol.unwrap_or_default() } else { symbol.to_string() };
    Ok(PriceSeries::new(name, bars))
}

// ---------------------------------------------------------------- client

/// Blocking Yahoo client holding the cookie jar (via the shared reqwest client) and crumb.
pub struct YahooClient {
    client: Client,
    crumb: Mutex<Option<String>>,
}

impl YahooClient {
    pub fn new(client: Client) -> Self {
        Self { client, crumb: Mutex::new(None) }
    }

    /// Obtain (and cache) the crumb. Errors are returned but callers may retry without one.
    pub fn crumb(&self) -> Result<String> {
        if let Some(c) = self.crumb.lock().unwrap().clone() {
            return Ok(c);
        }
        // fc.yahoo.com typically answers 404 but sets the A1/A3 cookies; ignore the status.
        let _ = self.client.get("https://fc.yahoo.com").send();
        let crumb = send_text(self.client.get(format!("{Q2}/v1/test/getcrumb")), "Yahoo crumb")?;
        let crumb = crumb.trim().to_string();
        if crumb.is_empty() || crumb.contains('<') || crumb.len() > 64 {
            return Err(anyhow!("Yahoo crumb: unexpected response '{}'", crumb.chars().take(60).collect::<String>()));
        }
        *self.crumb.lock().unwrap() = Some(crumb.clone());
        Ok(crumb)
    }

    fn reset_crumb(&self) {
        *self.crumb.lock().unwrap() = None;
    }

    /// GET a query endpoint with crumb; on 401/403 refresh the crumb once, then try query1.
    fn get_json(&self, path_and_query: &str, what: &str) -> Result<String> {
        let mut last_err = None;
        for attempt in 0..2 {
            let crumb = self.crumb().ok();
            for host in [Q2, Q1] {
                let mut url = format!("{host}{path_and_query}");
                if let Some(c) = &crumb {
                    url.push(if url.contains('?') { '&' } else { '?' });
                    url.push_str("crumb=");
                    url.push_str(c);
                }
                match send_text(self.client.get(&url).header("Accept", "application/json"), what) {
                    Ok(b) => return Ok(b),
                    Err(e) => last_err = Some(e),
                }
            }
            if attempt == 0 {
                self.reset_crumb();
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("{what}: failed")))
    }

    /// Fetch one options page; `date` = expiry UNIX timestamp (None = nearest expiry).
    pub fn options_page(&self, sym: &str, date: Option<i64>) -> Result<OptionsPage> {
        let ys = yahoo_symbol(sym);
        let mut p = format!("/v7/finance/options/{ys}");
        if let Some(d) = date {
            p.push_str(&format!("?date={d}"));
        }
        let body = self.get_json(&p, &format!("Yahoo options {ys}"))?;
        parse_options_page(&body)
    }

    /// Fetch the chain across expiries (all, or the nearest `max_expiries`).
    pub fn fetch_chain(&self, sym: &str, max_expiries: Option<usize>) -> Result<OptionChain> {
        let first = self.options_page(sym, None)?;
        let mut dates = first.expiration_dates.clone();
        dates.sort_unstable();
        if let Some(m) = max_expiries {
            dates.truncate(m.max(1));
        }
        let first_exp = first.quotes.first().map(|q| q.expiry);
        let mut pages = vec![first];
        for d in dates {
            if utc_date(d) == first_exp {
                continue;
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
            match self.options_page(sym, Some(d)) {
                Ok(p) => pages.push(p),
                Err(e) => {
                    // One failing expiry should not kill the whole chain.
                    eprintln!("optlab-data: Yahoo expiry {d} for {sym} skipped: {e:#}");
                }
            }
        }
        chain_from_pages(sym, &pages)
    }

    /// Daily history via the v8 chart endpoint.
    pub fn fetch_history(&self, sym: &str, start: Option<NaiveDate>, end: Option<NaiveDate>) -> Result<PriceSeries> {
        let ys = yahoo_symbol(sym);
        let p1 = start.map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp()).unwrap_or(0);
        let p2 = end
            .map(|d| (d + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
            .unwrap_or_else(|| chrono::Utc::now().timestamp() + 86_400);
        let path = format!("/v8/finance/chart/{ys}?period1={p1}&period2={p2}&interval=1d&events=div%2Csplit&includeAdjustedClose=true");
        let body = self.get_json(&path, &format!("Yahoo chart {ys}"))?;
        let s = parse_chart(&body, &clean_symbol(sym))?;
        Ok(s.slice(start, end))
    }

    /// Daily closes of an index (e.g. `^VIX`, `^IRX`) as a scalar series.
    pub fn fetch_scalar(&self, sym: &str, name: &str) -> Result<ScalarSeries> {
        let s = self.fetch_history(sym, None, None)?;
        Ok(ScalarSeries::new(name, s.bars.iter().map(|b| (b.date, b.close)).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn symbols() {
        assert_eq!(yahoo_symbol("SPX"), "^SPX");
        assert_eq!(yahoo_symbol("_VIX"), "^VIX");
        assert_eq!(yahoo_symbol("^irx"), "^IRX");
        assert_eq!(yahoo_symbol("IRX"), "^IRX");
        assert_eq!(yahoo_symbol("brk.b"), "BRK-B");
        assert_eq!(yahoo_symbol("qqq"), "QQQ");
    }
}
