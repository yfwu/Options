//! # optlab-data
//!
//! Market data for OptLab: live option chains (CBOE delayed quotes, Yahoo Finance,
//! Tradier, Polygon/Massive), CSV import, live and bundled historical series, a
//! local snapshot store and user configuration.
//!
//! Everything is **blocking**; GUI code should call [`DataService`] from a
//! background thread. `DataService` is `Send + Sync` and can be shared in an `Arc`.
//!
//! ```no_run
//! use optlab_data::{DataService, Provider};
//! let svc = DataService::load_default()?;
//! let chain = svc.fetch_chain_auto("QQQ")?;            // CBOE → Yahoo → Tradier/Polygon → snapshot
//! let hist = svc.fetch_history("QQQ", None, None)?;     // Yahoo → Stooq → bundled
//! let vxn = svc.fetch_vol_index("VXN")?;                // CBOE → Yahoo → bundled
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod bundled;
pub mod config;
pub mod csv_import;
pub mod history;
pub mod http;
pub mod providers;
pub mod store;
pub mod util;

pub use bundled::BundledInfo;
pub use config::Config;
pub use csv_import::ChainCsvOptions;
pub use store::{SnapshotInfo, Store};

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use optlab_core::{OptionChain, PriceSeries, ScalarSeries};
use providers::cboe::CboeQuote;
use providers::polygon::PolygonClient;
use providers::tradier::TradierClient;
use providers::yahoo::YahooClient;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Live option-chain providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// CBOE delayed quotes: free, no key, ~15 min delay, includes IV and greeks.
    Cboe,
    /// Yahoo Finance: free, unofficial, IV but no greeks.
    Yahoo,
    /// Tradier: needs a (free sandbox or brokerage) token; ORATS greeks.
    Tradier,
    /// Polygon.io / Massive: needs an API key; greeks and IV.
    Polygon,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Provider::Cboe, Provider::Yahoo, Provider::Tradier, Provider::Polygon];

    /// Machine id, also used as `OptionChain::source`.
    pub fn id(self) -> &'static str {
        match self {
            Provider::Cboe => "cboe",
            Provider::Yahoo => "yahoo",
            Provider::Tradier => "tradier",
            Provider::Polygon => "polygon",
        }
    }

    /// Human-readable name for menus.
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::Cboe => "CBOE delayed quotes (free, ~15 min delay)",
            Provider::Yahoo => "Yahoo Finance (free, unofficial)",
            Provider::Tradier => "Tradier (token required)",
            Provider::Polygon => "Polygon.io / Massive (API key required)",
        }
    }

    /// Short label.
    pub fn short_name(self) -> &'static str {
        match self {
            Provider::Cboe => "CBOE",
            Provider::Yahoo => "Yahoo",
            Provider::Tradier => "Tradier",
            Provider::Polygon => "Polygon",
        }
    }

    pub fn needs_key(self) -> bool {
        matches!(self, Provider::Tradier | Provider::Polygon)
    }

    /// Whether the provider returns greeks (otherwise the analysis layer computes them).
    pub fn has_greeks(self) -> bool {
        !matches!(self, Provider::Yahoo)
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.short_name())
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cboe" => Ok(Provider::Cboe),
            "yahoo" | "yf" => Ok(Provider::Yahoo),
            "tradier" => Ok(Provider::Tradier),
            "polygon" | "massive" => Ok(Provider::Polygon),
            other => Err(anyhow!("unknown provider '{other}' (cboe, yahoo, tradier, polygon)")),
        }
    }
}

/// Result of [`DataService::fetch_chain_auto_detailed`].
#[derive(Debug, Clone)]
pub struct ChainFetch {
    pub chain: OptionChain,
    /// Provider that answered; `None` when the chain came from a stored snapshot.
    pub provider: Option<Provider>,
    pub from_snapshot: bool,
    /// Error messages of providers tried before the one that succeeded.
    pub errors: Vec<String>,
}

/// Where a history series came from.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesFetch<T> {
    pub series: T,
    /// `"yahoo"`, `"stooq"`, `"cboe"`, `"fred"`, `"bundled"`, or `"cache:<origin>"`.
    pub source: String,
    pub errors: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Cached<T> {
    source: String,
    series: T,
}

/// Cache lifetime for downloaded histories.
pub const HISTORY_TTL: Duration = Duration::from_secs(12 * 3600);

/// Facade over providers, store, cache and bundled data.
pub struct DataService {
    config: Config,
    store: Store,
    client: Client,
    yahoo: YahooClient,
    offline: AtomicBool,
}

impl DataService {
    /// Build with an explicit config. The store root is `config.data_dir` or the default.
    pub fn new(config: Config) -> Result<Self> {
        let store = match &config.data_dir {
            Some(d) => Store::at(d),
            None => Store::open_default()?,
        };
        Self::with_store(config, store)
    }

    /// Build with an explicit store (useful for tests / portable installs).
    pub fn with_store(config: Config, store: Store) -> Result<Self> {
        let client = http::build_client()?;
        Ok(Self { yahoo: YahooClient::new(client.clone()), client, config, store, offline: AtomicBool::new(false) })
    }

    /// Load `config.toml` + env overrides and build.
    pub fn load_default() -> Result<Self> {
        Self::new(Config::load()?)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Shared HTTP client (cookies, gzip, 15 s timeout).
    pub fn http_client(&self) -> &Client {
        &self.client
    }

    /// Offline mode: skip all network calls (snapshots / cache / bundled only).
    pub fn set_offline(&self, offline: bool) {
        self.offline.store(offline, Ordering::Relaxed);
    }

    pub fn is_offline(&self) -> bool {
        self.offline.load(Ordering::Relaxed)
    }

    fn online(&self) -> Result<()> {
        if self.is_offline() {
            Err(anyhow!("offline mode"))
        } else {
            Ok(())
        }
    }

    /// Providers usable with the current config (keys present), in auto order.
    pub fn available_providers(&self) -> Vec<Provider> {
        let usable = |p: &Provider| match p {
            Provider::Tradier => self.config.has_tradier(),
            Provider::Polygon => self.config.has_polygon(),
            _ => true,
        };
        let mut v: Vec<Provider> = self.config.default_provider.into_iter().filter(usable).collect();
        for p in Provider::ALL.into_iter().filter(usable) {
            if !v.contains(&p) {
                v.push(p);
            }
        }
        v
    }

    fn fetch_chain_raw(&self, provider: Provider, symbol: &str) -> Result<OptionChain> {
        self.online()?;
        let max = self.config.max_expiries;
        match provider {
            Provider::Cboe => providers::cboe::fetch_chain(&self.client, symbol),
            Provider::Yahoo => self.yahoo.fetch_chain(symbol, max),
            Provider::Tradier => {
                let tok = self.config.tradier_token.clone().unwrap_or_default();
                TradierClient::new(self.client.clone(), tok, self.config.tradier_sandbox).fetch_chain(symbol, max)
            }
            Provider::Polygon => {
                let key = self.config.polygon_api_key.clone().unwrap_or_default();
                let pc = PolygonClient::new(self.client.clone(), key);
                let snap = pc.fetch_snapshot(symbol)?;
                let hint = if snap.underlying_price.is_none() { self.spot_hint(symbol) } else { None };
                providers::polygon::chain_from_snapshot(symbol, snap, hint)
            }
        }
    }

    fn spot_hint(&self, symbol: &str) -> Option<f64> {
        providers::cboe::fetch_quote(&self.client, symbol)
            .map(|q| q.price)
            .or_else(|_| self.yahoo.options_page(symbol, None).map(|p| p.spot))
            .ok()
    }

    /// Fetch a full chain from one provider and save a snapshot (save errors are ignored).
    pub fn fetch_chain(&self, provider: Provider, symbol: &str) -> Result<OptionChain> {
        let chain = self
            .fetch_chain_raw(provider, symbol)
            .with_context(|| format!("{} chain for {} failed", provider.short_name(), symbol.trim()))?;
        if let Err(e) = self.store.save_chain(&chain) {
            eprintln!("optlab-data: could not save snapshot: {e:#}");
        }
        Ok(chain)
    }

    /// Try providers in order (default provider, CBOE, Yahoo, Tradier/Polygon when
    /// configured), falling back to the latest stored snapshot.
    pub fn fetch_chain_auto(&self, symbol: &str) -> Result<OptionChain> {
        Ok(self.fetch_chain_auto_detailed(symbol)?.chain)
    }

    /// Like [`fetch_chain_auto`](Self::fetch_chain_auto) but reports which source answered.
    pub fn fetch_chain_auto_detailed(&self, symbol: &str) -> Result<ChainFetch> {
        let mut errors = Vec::new();
        if !self.is_offline() {
            for p in self.available_providers() {
                match self.fetch_chain(p, symbol) {
                    Ok(chain) => return Ok(ChainFetch { chain, provider: Some(p), from_snapshot: false, errors }),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
        } else {
            errors.push("offline mode".to_string());
        }
        match self.store.latest_snapshot(symbol) {
            Ok(Some(chain)) => Ok(ChainFetch { chain, provider: None, from_snapshot: true, errors }),
            Ok(None) => Err(anyhow!(
                "no option chain for {}: all providers failed and no stored snapshot.\n  - {}",
                symbol.trim(),
                errors.join("\n  - ")
            )),
            Err(e) => Err(e.context(format!("providers failed: {}", errors.join("; ")))),
        }
    }

    /// Delayed CBOE quote of the underlying (price and IV30 in vol points).
    pub fn fetch_cboe_quote(&self, symbol: &str) -> Result<CboeQuote> {
        self.online()?;
        providers::cboe::fetch_quote(&self.client, symbol)
    }

    /// Latest stored snapshot for a symbol.
    pub fn latest_snapshot(&self, symbol: &str) -> Result<Option<OptionChain>> {
        self.store.latest_snapshot(symbol)
    }

    pub fn list_snapshots(&self, symbol: Option<&str>) -> Result<Vec<SnapshotInfo>> {
        self.store.list_snapshots(symbol)
    }

    /// Import a chain CSV and save it as a snapshot (`source = "csv"`).
    pub fn import_chain_csv(&self, path: impl AsRef<Path>, opts: &ChainCsvOptions) -> Result<OptionChain> {
        let chain = csv_import::load_chain_csv(path, opts)?;
        self.store.save_chain(&chain)?;
        Ok(chain)
    }

    /// Daily history (Yahoo → Stooq → bundled), cached for 12 h.
    pub fn fetch_history(&self, symbol: &str, start: Option<NaiveDate>, end: Option<NaiveDate>) -> Result<PriceSeries> {
        Ok(self.fetch_history_detailed(symbol, start, end)?.series)
    }

    pub fn fetch_history_detailed(
        &self,
        symbol: &str,
        start: Option<NaiveDate>,
        end: Option<NaiveDate>,
    ) -> Result<SeriesFetch<PriceSeries>> {
        let sym = providers::clean_symbol(symbol);
        let key = format!("hist_{sym}");
        let mut errors = Vec::new();
        if let Some(c) = self.store.cache_get::<Cached<PriceSeries>>(&key, Some(HISTORY_TTL)) {
            return Ok(SeriesFetch { series: c.series.slice(start, end), source: format!("cache:{}", c.source), errors });
        }
        let mut live: Vec<(&str, Result<PriceSeries>)> = Vec::new();
        if self.is_offline() {
            errors.push("offline mode".into());
        } else {
            live.push(("yahoo", self.yahoo.fetch_history(symbol, None, None)));
            if live[0].1.is_err() {
                live.push(("stooq", history::fetch_stooq(&self.client, symbol)));
            }
        }
        for (src, r) in live {
            match r {
                Ok(s) if !s.is_empty() => {
                    let _ = self.store.cache_put(&key, &Cached { source: src.to_string(), series: s.clone() });
                    return Ok(SeriesFetch { series: s.slice(start, end), source: src.to_string(), errors });
                }
                Ok(_) => errors.push(format!("{src}: empty series")),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        if let Some(b) = bundled::prices(&sym) {
            return Ok(SeriesFetch { series: b.slice(start, end), source: "bundled".into(), errors });
        }
        Err(anyhow!("no price history for {sym}:\n  - {}", errors.join("\n  - ")))
    }

    /// Volatility index (`VIX`, `VXN`, `VIX3M`, ...): CBOE CSV → Yahoo → bundled.
    /// Live data is merged with the bundled series so early history is kept.
    pub fn fetch_vol_index(&self, name: &str) -> Result<ScalarSeries> {
        Ok(self.fetch_vol_index_detailed(name)?.series)
    }

    pub fn fetch_vol_index_detailed(&self, name: &str) -> Result<SeriesFetch<ScalarSeries>> {
        let n = providers::clean_symbol(name);
        let key = format!("vol_{n}");
        let mut errors = Vec::new();
        if let Some(c) = self.store.cache_get::<Cached<ScalarSeries>>(&key, Some(HISTORY_TTL)) {
            return Ok(SeriesFetch { series: c.series, source: format!("cache:{}", c.source), errors });
        }
        let bundled = bundled::vol_index(&n);
        if !self.is_offline() {
            let attempts: [(&str, &dyn Fn() -> Result<ScalarSeries>); 2] = [
                ("cboe", &|| history::fetch_cboe_index(&self.client, &n)),
                ("yahoo", &|| self.yahoo.fetch_scalar(&format!("^{n}"), &n)),
            ];
            for (src, f) in attempts {
                match f() {
                    Ok(s) if !s.is_empty() => {
                        let merged = match &bundled {
                            Some(b) => bundled::merge_scalar(&n, &[&s, b]),
                            None => s,
                        };
                        let _ = self.store.cache_put(&key, &Cached { source: src.to_string(), series: merged.clone() });
                        return Ok(SeriesFetch { series: merged, source: src.to_string(), errors });
                    }
                    Ok(_) => errors.push(format!("{src}: empty series")),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
        } else {
            errors.push("offline mode".into());
        }
        match bundled {
            Some(series) => Ok(SeriesFetch { series, source: "bundled".into(), errors }),
            None => Err(anyhow!("no volatility index {n}:\n  - {}", errors.join("\n  - "))),
        }
    }

    /// Interest-rate series in percent (default `DGS3MO`): FRED → Yahoo `^IRX` → bundled.
    pub fn fetch_rate(&self, series_id: &str) -> Result<SeriesFetch<ScalarSeries>> {
        let id = series_id.trim().to_ascii_uppercase();
        let key = format!("rate_{id}");
        let mut errors = Vec::new();
        if let Some(c) = self.store.cache_get::<Cached<ScalarSeries>>(&key, Some(HISTORY_TTL)) {
            return Ok(SeriesFetch { series: c.series, source: format!("cache:{}", c.source), errors });
        }
        if !self.is_offline() {
            let r = history::fetch_fred(&self.client, &id).map(|s| ("fred", s)).or_else(|e| {
                errors.push(format!("{e:#}"));
                if id == "DGS3MO" {
                    // ^IRX is the 13-week T-bill discount yield in percent.
                    self.yahoo.fetch_scalar("^IRX", "IRX").map(|s| ("yahoo", s))
                } else {
                    Err(anyhow!("no Yahoo fallback for {id}"))
                }
            });
            match r {
                Ok((src, s)) if !s.is_empty() => {
                    let _ = self.store.cache_put(&key, &Cached { source: src.to_string(), series: s.clone() });
                    return Ok(SeriesFetch { series: s, source: src.to_string(), errors });
                }
                Ok(_) => errors.push("empty rate series".into()),
                Err(e) => errors.push(format!("{e:#}")),
            }
        } else {
            errors.push("offline mode".into());
        }
        bundled::rate(&id)
            .map(|series| SeriesFetch { series, source: "bundled".into(), errors: errors.clone() })
            .ok_or_else(|| anyhow!("no rate series {id}:\n  - {}", errors.join("\n  - ")))
    }

    /// Risk-free rate as a decimal: config override, else latest 3M T-bill
    /// (FRED/Yahoo/bundled), else 4%.
    pub fn risk_free_rate(&self) -> f64 {
        if let Some(r) = self.config.risk_free_rate {
            return r;
        }
        self.fetch_rate("DGS3MO")
            .ok()
            .and_then(|f| f.series.last())
            .map(|(_, v)| v / 100.0)
            .unwrap_or(0.04)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_roundtrip() {
        for p in Provider::ALL {
            assert_eq!(p.id().parse::<Provider>().unwrap(), p);
        }
        assert_eq!(serde_json::to_string(&Provider::Cboe).unwrap(), "\"cboe\"");
        assert!("x".parse::<Provider>().is_err());
    }
}
