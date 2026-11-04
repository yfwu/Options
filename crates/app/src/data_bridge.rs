//! Adapter between the GUI and `optlab-data` (bundled data, providers, config,
//! snapshots) plus a few GUI-only helpers (demo chain, app-specific settings).

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Weekday};
use optlab_core::backtest::{approx_us_tbill_rates, BacktestData, RateSource};
use optlab_core::pricing::{bs_greeks, year_frac};
use optlab_core::strategy::{strike_step, third_friday};
use optlab_core::synth::SynthVolModel;
use optlab_core::{ExerciseStyle, OptionChain, OptionKind, OptionQuote, PriceSeries, ScalarSeries};
use optlab_data::{bundled, Config, DataService, Provider, Store};
use serde::{Deserialize, Serialize};

/// Settings edited on the 設定 page: a superset of `optlab_data::Config`.
#[derive(Clone, Debug)]
pub struct Settings {
    pub config: Config,
    /// Effective risk-free rate (decimal) — manual override or latest bundled 3M T-bill.
    pub risk_free: f64,
    pub dividend_yield: f64,
    pub default_symbol: String,
}

/// App-only settings persisted next to `config.toml` (`app.json`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppExtras {
    pub dividend_yield: f64,
    pub default_symbol: String,
}

impl Default for AppExtras {
    fn default() -> Self {
        Self { dividend_yield: 0.006, default_symbol: "QQQ".into() }
    }
}

fn extras_path() -> Option<PathBuf> {
    Config::default_path().ok().map(|p| p.with_file_name("app.json"))
}

/// Latest bundled 3-month T-bill yield (decimal).
pub fn latest_tbill() -> Option<f64> {
    bundled::rate("DGS3MO").and_then(|s| s.last()).map(|(_, v)| v / 100.0)
}

pub fn load_settings() -> Settings {
    let config = Config::load().unwrap_or_default();
    let extras: AppExtras = extras_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let risk_free = config.risk_free_rate.or_else(latest_tbill).unwrap_or(0.04);
    Settings { config, risk_free, dividend_yield: extras.dividend_yield, default_symbol: extras.default_symbol }
}

pub fn save_settings(s: &Settings) -> Result<PathBuf, String> {
    let p = s.config.save().map_err(|e| format!("{e:#}"))?;
    if let Some(ep) = extras_path() {
        let ex = AppExtras { dividend_yield: s.dividend_yield, default_symbol: s.default_symbol.clone() };
        let txt = serde_json::to_string_pretty(&ex).map_err(|e| e.to_string())?;
        std::fs::write(&ep, txt).map_err(|e| format!("{}: {e}", ep.display()))?;
    }
    Ok(p)
}

/// Build a data service for the given config (never fails: falls back to a temp store).
pub fn service(config: &Config) -> Arc<DataService> {
    match DataService::new(config.clone()) {
        Ok(s) => Arc::new(s),
        Err(_) => {
            let root = std::env::temp_dir().join("optlab");
            Arc::new(DataService::with_store(config.clone(), Store::at(root)).expect("http client"))
        }
    }
}

pub fn bundled_symbols() -> Vec<String> {
    bundled::price_symbols().into_iter().map(|s| s.to_string()).collect()
}

pub fn bundled_prices(symbol: &str) -> Result<PriceSeries, String> {
    bundled::prices(symbol).ok_or_else(|| format!("沒有 {symbol} 的內建歷史價格"))
}

pub fn bundled_vol_index(name: &str) -> Result<ScalarSeries, String> {
    bundled::vol_index(name).ok_or_else(|| format!("沒有 {name} 的內建波動率指數"))
}

/// The natural vol index for an underlying (VXN with VIX proxy back-fill for Nasdaq names).
pub fn default_vol_for(symbol: &str) -> Result<(String, ScalarSeries), String> {
    let name = bundled::default_vol_index_name(symbol).to_string();
    bundled::default_vol_index_for(symbol).map(|s| (name.clone(), s)).ok_or_else(|| format!("沒有 {name} 內建資料"))
}

/// Risk-free rate history for backtests: bundled DGS3MO spliced onto the approximate
/// 1990+ T-bill table for early dates.
pub fn rate_source() -> RateSource {
    let approx = approx_us_tbill_rates();
    match bundled::rate("DGS3MO") {
        Some(r) if !r.is_empty() => {
            let first = r.points[0].0;
            let mut pts: Vec<(NaiveDate, f64)> = approx.points.iter().copied().filter(|(d, _)| *d < first).collect();
            pts.extend(r.points.iter().copied());
            RateSource::Series(ScalarSeries::new("3M T-bill", pts))
        }
        _ => RateSource::Series(approx),
    }
}

pub fn default_dividend_yield(symbol: &str) -> f64 {
    match symbol.to_ascii_uppercase().as_str() {
        "SPY" | "VTI" | "SPX" => 0.014,
        "QQQ" | "NDX" => 0.006,
        "MSFT" => 0.008,
        "AVGO" => 0.013,
        "TSM" => 0.015,
        "SOXX" => 0.007,
        _ => 0.0,
    }
}

/// Assemble backtest inputs from bundled data.
pub fn backtest_data(symbol: &str, q: f64) -> Result<(BacktestData, String), String> {
    let prices = bundled_prices(symbol)?;
    let (vname, vol) = default_vol_for(symbol)?;
    Ok((BacktestData::new(prices, vol, rate_source(), q), vname))
}

pub fn vol_model_for(symbol: &str) -> SynthVolModel {
    if bundled::default_vol_index_name(symbol) == "VXN" {
        SynthVolModel::qqq()
    } else {
        SynthVolModel::spy()
    }
}

/// Provider choices of the chain page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainSource {
    Auto,
    Live(Provider),
    Snapshot,
    Csv,
    Demo,
    CboeSample,
}

impl ChainSource {
    pub const ALL: [ChainSource; 9] = [
        ChainSource::Auto,
        ChainSource::Live(Provider::Cboe),
        ChainSource::Live(Provider::Yahoo),
        ChainSource::Live(Provider::Tradier),
        ChainSource::Live(Provider::Polygon),
        ChainSource::Snapshot,
        ChainSource::Csv,
        ChainSource::Demo,
        ChainSource::CboeSample,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ChainSource::Auto => "自動 (CBOE→Yahoo→…→快照)",
            ChainSource::Live(Provider::Cboe) => "CBOE 延遲報價",
            ChainSource::Live(Provider::Yahoo) => "Yahoo Finance",
            ChainSource::Live(Provider::Tradier) => "Tradier (需 token)",
            ChainSource::Live(Provider::Polygon) => "Polygon (需 key)",
            ChainSource::Snapshot => "最新快照",
            ChainSource::Csv => "CSV 檔案路徑",
            ChainSource::Demo => "合成示範資料",
            ChainSource::CboeSample => "CBOE 範例檔 (DOV)",
        }
    }
}

const CBOE_SAMPLE: &str = include_str!("../../data/tests/fixtures/cboe_delayed_DOV_2026-09-10.json");

/// Blocking chain load; call from a background thread.
pub fn load_chain(svc: &DataService, source: ChainSource, symbol: &str, csv_path: &str) -> Result<(OptionChain, String), String> {
    let sym = symbol.trim().to_ascii_uppercase();
    match source {
        ChainSource::Auto => svc
            .fetch_chain_auto_detailed(&sym)
            .map(|f| {
                let from = match (f.provider, f.from_snapshot) {
                    (Some(p), _) => format!("即時 {}", p.short_name()),
                    (None, true) => format!("快照 (即時來源失敗 {} 次)", f.errors.len()),
                    _ => "未知".into(),
                };
                (f.chain, from)
            })
            .map_err(|e| format!("{e:#}")),
        ChainSource::Live(p) => svc.fetch_chain(p, &sym).map(|c| (c, format!("即時 {}", p.short_name()))).map_err(|e| format!("{e:#}")),
        ChainSource::Snapshot => match svc.latest_snapshot(&sym) {
            Ok(Some(c)) => Ok((c, "本地快照".into())),
            Ok(None) => Err(format!("沒有 {sym} 的本地快照；請先即時取得或匯入 CSV（或使用合成示範資料）")),
            Err(e) => Err(format!("{e:#}")),
        },
        ChainSource::Csv => {
            let path = csv_path.trim();
            if path.is_empty() {
                return Err("請輸入 CSV 檔案路徑".into());
            }
            let opts = optlab_data::ChainCsvOptions {
                underlying: (!sym.is_empty()).then(|| sym.clone()),
                ..Default::default()
            };
            svc.import_chain_csv(path, &opts).map(|c| (c, format!("CSV {path}"))).map_err(|e| format!("{e:#}"))
        }
        ChainSource::Demo => demo_chain(if sym.is_empty() { "QQQ" } else { &sym }).map(|c| (c, "合成示範資料".into())),
        ChainSource::CboeSample => optlab_data::providers::cboe::parse_chain(CBOE_SAMPLE, "DOV")
            .map(|c| (c, "CBOE 範例檔 (DOV 2026-09-10)".into()))
            .map_err(|e| format!("{e:#}")),
    }
}

/// Build a realistic synthetic chain (合成示範資料) from the last bundled close, the
/// matching vol index level and the calibrated skewed vol surface.
pub fn demo_chain(symbol: &str) -> Result<OptionChain, String> {
    let sym = symbol.trim().to_ascii_uppercase();
    let (spot, as_of_date, vix) = match bundled::prices(&sym) {
        Some(p) if !p.is_empty() => {
            let last = *p.bars.last().unwrap();
            let v = default_vol_for(&sym).ok().and_then(|(_, s)| s.value_at(last.date)).unwrap_or(20.0);
            (last.close, last.date, v)
        }
        _ => {
            // index / unknown symbol: scale from a known proxy
            let (proxy, mult) = match sym.as_str() {
                "SPX" | "XSP" => ("SPY", if sym == "SPX" { 10.0 } else { 1.0 }),
                "NDX" => ("QQQ", 41.0),
                _ => ("QQQ", 1.0),
            };
            let p = bundled::prices(proxy).ok_or("no bundled data")?;
            let last = *p.bars.last().unwrap();
            let v = default_vol_for(proxy).ok().and_then(|(_, s)| s.value_at(last.date)).unwrap_or(20.0);
            (last.close * mult, last.date, v)
        }
    };
    let model = vol_model_for(&sym);
    let r = latest_tbill().unwrap_or(0.04);
    let q = default_dividend_yield(&sym);
    let as_of = as_of_date.and_hms_opt(16, 0, 0).unwrap_or(NaiveDateTime::default());

    // expiries: next 4 Fridays, monthly 3rd Fridays for 12 months, LEAPS Dec/Jan out ~2.5y
    let mut exps: Vec<NaiveDate> = vec![];
    let mut d = as_of_date + Duration::days(1);
    while exps.len() < 4 {
        if d.weekday() == Weekday::Fri {
            exps.push(d);
        }
        d += Duration::days(1);
    }
    for k in 0..13 {
        let idx = as_of_date.year() * 12 + as_of_date.month0() as i32 + k;
        let e = third_friday(idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32);
        if e > as_of_date {
            exps.push(e);
        }
    }
    for y in 1..=3 {
        for m in [1u32, 6, 12] {
            let e = third_friday(as_of_date.year() + y, m);
            if (e - as_of_date).num_days() <= 365 * 3 {
                exps.push(e);
            }
        }
    }
    exps.sort();
    exps.dedup();

    let step = strike_step(spot);
    let mut quotes = vec![];
    for e in exps {
        let t = year_frac(as_of_date, e).max(1.0 / 365.0);
        let atm = model.atm_vol(t, vix);
        let width = (3.2 * atm * t.sqrt()).clamp(0.08, 0.85);
        let estep = if t > 0.5 { step * 2.0 } else { step };
        let lo = ((spot * (1.0 - width)) / estep).floor() * estep;
        let hi = ((spot * (1.0 + width * 0.8)) / estep).ceil() * estep;
        let mut k = lo.max(estep);
        while k <= hi + 1e-9 {
            for kind in [OptionKind::Call, OptionKind::Put] {
                let iv = model.iv(k, spot, t, vix);
                let g = bs_greeks(kind, spot, k, t, r, q, iv);
                if g.price < 0.01 {
                    continue;
                }
                let half = (g.price * 0.012).max(0.02).min(g.price * 0.2 + 0.05);
                let mny = ((k / spot).ln() / (atm * t.sqrt()).max(1e-6)).abs();
                let oi = (20000.0 * (-0.5 * mny * mny).exp() / (1.0 + 2.0 * t) + 50.0).round();
                let vol = (oi * 0.08 / (1.0 + 4.0 * t)).round();
                let contract = format!(
                    "{}{}{}{:08}",
                    sym,
                    e.format("%y%m%d"),
                    kind.label(),
                    (k * 1000.0).round() as u64
                );
                quotes.push(OptionQuote {
                    contract,
                    expiry: e,
                    strike: k,
                    kind,
                    bid: ((g.price - half) * 100.0).round() / 100.0,
                    ask: ((g.price + half) * 100.0).round() / 100.0,
                    last: (g.price * 100.0).round() / 100.0,
                    volume: vol,
                    open_interest: oi,
                    iv: Some(iv),
                    delta: Some(g.delta),
                    gamma: Some(g.gamma),
                    theta: Some(g.theta),
                    vega: Some(g.vega),
                });
            }
            k += estep;
        }
    }
    let style = if matches!(sym.as_str(), "SPX" | "NDX" | "XSP" | "XND") { ExerciseStyle::European } else { ExerciseStyle::American };
    Ok(OptionChain { underlying: sym, spot, as_of, source: "demo".into(), style, quotes })
}
