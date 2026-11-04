//! Polygon.io (now "Massive") options chain snapshot (requires API key; the
//! free tier is end-of-day and may omit `last_quote`).
//!
//! `GET https://api.polygon.io/v3/snapshot/options/{UNDERLYING}?limit=250&apiKey=KEY`,
//! then follow `next_url` (which does not carry the key) until absent.
//! Index underlyings use the `I:` prefix (`I:SPX`).

use super::{clean_symbol, finite, is_index_symbol, positive, style_for};
use crate::http::{redact, send_text};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime};
use optlab_core::chain::parse_occ_symbol;
use optlab_core::{OptionChain, OptionKind, OptionQuote};
use reqwest::blocking::Client;
use serde::Deserialize;

pub const SOURCE: &str = "polygon";
pub const DEFAULT_BASE: &str = "https://api.polygon.io";
/// Safety cap on pagination (250 contracts/page → 50 000 contracts).
pub const MAX_PAGES: usize = 200;

pub fn polygon_underlying(sym: &str) -> String {
    let c = clean_symbol(sym);
    if is_index_symbol(&c) {
        format!("I:{c}")
    } else {
        c
    }
}

#[derive(Debug, Deserialize)]
struct Page {
    status: Option<String>,
    error: Option<String>,
    message: Option<String>,
    #[serde(default)]
    results: Vec<Snap>,
    next_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Snap {
    details: Option<Details>,
    day: Option<Day>,
    greeks: Option<Greeks>,
    implied_volatility: Option<f64>,
    last_quote: Option<LastQuote>,
    last_trade: Option<LastTrade>,
    open_interest: Option<f64>,
    underlying_asset: Option<Underlying>,
}
#[derive(Debug, Deserialize)]
struct Details {
    contract_type: Option<String>,
    expiration_date: Option<String>,
    strike_price: Option<f64>,
    ticker: Option<String>,
}
#[derive(Debug, Deserialize)]
struct Day {
    close: Option<f64>,
    volume: Option<f64>,
    last_updated: Option<i64>,
}
#[derive(Debug, Deserialize)]
struct Greeks {
    delta: Option<f64>,
    gamma: Option<f64>,
    theta: Option<f64>,
    vega: Option<f64>,
}
#[derive(Debug, Deserialize)]
struct LastQuote {
    bid: Option<f64>,
    ask: Option<f64>,
    midpoint: Option<f64>,
}
#[derive(Debug, Deserialize)]
struct LastTrade {
    price: Option<f64>,
}
#[derive(Debug, Deserialize)]
struct Underlying {
    price: Option<f64>,
    value: Option<f64>,
    last_updated: Option<i64>,
}

/// Parsed snapshot page.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotPage {
    pub quotes: Vec<OptionQuote>,
    pub underlying_price: Option<f64>,
    /// Latest `underlying_asset.last_updated` (New York time).
    pub as_of: Option<NaiveDateTime>,
    pub next_url: Option<String>,
}

fn ns_to_ny(ns: i64) -> Option<NaiveDateTime> {
    DateTime::from_timestamp(ns / 1_000_000_000, 0).map(|d| crate::util::unix_to_new_york(d.timestamp()))
}

/// Parse one `/v3/snapshot/options` page.
pub fn parse_snapshot_page(json: &str) -> Result<SnapshotPage> {
    let p: Page = serde_json::from_str(json).context("Polygon: unexpected snapshot JSON shape")?;
    if let Some(st) = &p.status {
        if st != "OK" && st != "DELAYED" && p.results.is_empty() {
            let msg = p.error.or(p.message).unwrap_or_default();
            return Err(anyhow!("Polygon: status {st}: {msg}"));
        }
    }
    let mut quotes = Vec::with_capacity(p.results.len());
    let mut up = None;
    let mut as_of: Option<NaiveDateTime> = None;
    for s in p.results {
        if let Some(u) = &s.underlying_asset {
            if up.is_none() {
                up = positive(u.price).or(positive(u.value));
            }
            if let Some(t) = u.last_updated.and_then(ns_to_ny) {
                as_of = Some(as_of.map_or(t, |a| a.max(t)));
            }
        }
        let Some(d) = s.details else { continue };
        let ticker = d.ticker.unwrap_or_default();
        let occ = parse_occ_symbol(&ticker);
        let kind = match d.contract_type.as_deref() {
            Some("call") => OptionKind::Call,
            Some("put") => OptionKind::Put,
            _ => match &occ {
                Some(p) => p.2,
                None => continue,
            },
        };
        let expiry = d
            .expiration_date
            .as_deref()
            .and_then(|e| NaiveDate::parse_from_str(e, "%Y-%m-%d").ok())
            .or(occ.as_ref().map(|p| p.1));
        let (Some(expiry), Some(strike)) = (expiry, d.strike_price.or(occ.as_ref().map(|p| p.3))) else { continue };
        let lq = s.last_quote.as_ref();
        let (mut bid, mut ask) = (lq.and_then(|q| q.bid).unwrap_or(0.0), lq.and_then(|q| q.ask).unwrap_or(0.0));
        if bid <= 0.0 && ask <= 0.0 {
            if let Some(m) = positive(lq.and_then(|q| q.midpoint)) {
                bid = m;
                ask = m;
            }
        }
        let last = positive(s.last_trade.as_ref().and_then(|t| t.price))
            .or(positive(s.day.as_ref().and_then(|d| d.close)))
            .unwrap_or(0.0);
        if as_of.is_none() {
            as_of = s.day.as_ref().and_then(|d| d.last_updated).and_then(ns_to_ny);
        }
        let g = s.greeks.as_ref();
        quotes.push(OptionQuote {
            contract: ticker.trim_start_matches("O:").to_string(),
            expiry,
            strike,
            kind,
            bid: bid.max(0.0),
            ask: ask.max(0.0),
            last,
            volume: s.day.as_ref().and_then(|d| d.volume).unwrap_or(0.0),
            open_interest: s.open_interest.unwrap_or(0.0),
            iv: positive(s.implied_volatility),
            delta: finite(g.and_then(|g| g.delta)),
            gamma: finite(g.and_then(|g| g.gamma)),
            theta: finite(g.and_then(|g| g.theta)),
            vega: finite(g.and_then(|g| g.vega)),
        });
    }
    Ok(SnapshotPage { quotes, underlying_price: up, as_of, next_url: p.next_url.filter(|u| !u.is_empty()) })
}

/// Blocking Polygon client.
pub struct PolygonClient {
    client: Client,
    api_key: String,
    base: String,
}

impl PolygonClient {
    pub fn new(client: Client, api_key: impl Into<String>) -> Self {
        Self { client, api_key: api_key.into(), base: DEFAULT_BASE.to_string() }
    }

    /// Use another base URL (e.g. `https://api.massive.com`).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into().trim_end_matches('/').to_string();
        self
    }

    fn get(&self, url: &str) -> Result<String> {
        if self.api_key.trim().is_empty() {
            return Err(anyhow!("Polygon: no API key configured (set POLYGON_API_KEY or config.toml polygon_api_key)"));
        }
        let sep = if url.contains('?') { '&' } else { '?' };
        let full = format!("{url}{sep}apiKey={}", self.api_key.trim());
        send_text(self.client.get(&full).header("Accept", "application/json"), &format!("Polygon {}", redact(url)))
    }

    /// Full chain via paginated snapshot. `spot_hint` is used when the plan
    /// does not include `underlying_asset.price`.
    pub fn fetch_chain(&self, sym: &str, spot_hint: Option<f64>) -> Result<OptionChain> {
        let u = polygon_underlying(sym);
        let mut url = format!("{}/v3/snapshot/options/{u}?limit=250", self.base);
        let mut quotes = Vec::new();
        let mut spot = None;
        let mut as_of = None;
        for _ in 0..MAX_PAGES {
            let page = parse_snapshot_page(&self.get(&url)?)?;
            quotes.extend(page.quotes);
            spot = spot.or(page.underlying_price);
            as_of = match (as_of, page.as_of) {
                (Some(a), Some(b)) => Some(std::cmp::max(a, b)),
                (a, b) => a.or(b),
            };
            match page.next_url {
                Some(n) => url = n,
                None => break,
            }
        }
        if quotes.is_empty() {
            return Err(anyhow!("Polygon: snapshot for {u} returned no contracts"));
        }
        let spot = spot
            .or(spot_hint)
            .ok_or_else(|| anyhow!("Polygon: snapshot for {u} has no underlying price (plan without underlying data?)"))?;
        let underlying = clean_symbol(sym);
        Ok(OptionChain {
            style: style_for(&underlying),
            underlying,
            spot,
            as_of: as_of.unwrap_or_else(crate::util::now_new_york),
            source: SOURCE.to_string(),
            quotes,
        })
    }
}
