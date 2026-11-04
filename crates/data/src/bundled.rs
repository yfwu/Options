//! Historical data compiled into the binary (see `data/README.md` for provenance).
//!
//! All series come from public third-party GitHub mirrors of Yahoo Finance,
//! CBOE, Barchart and FRED data. They are a convenient offline fallback; refresh
//! with the live providers on your own machine for trading decisions.

use crate::csv_import::{parse_price_csv, parse_scalar_csv};
use chrono::NaiveDate;
use optlab_core::{Bar, PriceSeries, ScalarSeries};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

macro_rules! data_file {
    ($name:literal) => {
        include_str!(concat!("../../../data/", $name))
    };
}

const QQQ_ADJ_1999: &str = data_file!("QQQ_adj_1999.csv");
const VIX_SPY_1993: &str = data_file!("VIX_SPY_1993.csv");
const SPY_2019: &str = data_file!("SPY_2019.csv");
const VIX_HISTORY: &str = data_file!("VIX_History.csv");
const VIX_CLOSE_RECENT: &str = data_file!("VIX_close_recent.csv");
const VIX3M_HISTORY: &str = data_file!("VIX3M_History.csv");
const VXN_HISTORY: &str = data_file!("VXN_History.csv");
const VXN_BARCHART: &str = data_file!("VXN_barchart_2000.csv");
const DGS3MO: &str = data_file!("DGS3MO.csv");

/// `(symbol, csv, description)` for the yennanliu/finance_data 10-year files.
const TEN_YEAR: &[(&str, &str, &str)] = &[
    ("QQQ", data_file!("QQQ_10y.csv"), "Invesco QQQ Trust (Nasdaq-100 ETF)"),
    ("NVDA", data_file!("NVDA_10y.csv"), "NVIDIA Corp"),
    ("MSFT", data_file!("MSFT_10y.csv"), "Microsoft Corp"),
    ("GOOG", data_file!("GOOG_10y.csv"), "Alphabet Inc. class C"),
    ("AMZN", data_file!("AMZN_10y.csv"), "Amazon.com Inc"),
    ("META", data_file!("META_10y.csv"), "Meta Platforms Inc"),
    ("AVGO", data_file!("AVGO_10y.csv"), "Broadcom Inc"),
    ("TSM", data_file!("TSM_10y.csv"), "Taiwan Semiconductor ADR"),
    ("VTI", data_file!("VTI_10y.csv"), "Vanguard Total Stock Market ETF"),
    ("SOXX", data_file!("SOXX_10y.csv"), "iShares Semiconductor ETF"),
];

const SRC_YENNANLIU: &str = "https://github.com/yennanliu/finance_data (Yahoo Finance mirror)";

/// Kind of bundled dataset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BundledKind {
    /// Daily OHLC / adjusted close.
    Prices,
    /// Volatility index level in vol points (VIX, VXN, VIX3M).
    VolIndex,
    /// Interest rate in percent (DGS3MO).
    Rate,
}

/// Description of one bundled dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundledInfo {
    pub symbol: String,
    pub kind: BundledKind,
    pub description: String,
    pub first: NaiveDate,
    pub last: NaiveDate,
    pub source: String,
}

// ------------------------------------------------------------------ VXN proxy

/// Median VXN/VIX ratio over 2000-10-10 .. 2002-12-31 (502 overlapping days,
/// Barchart VXN vs CBOE VIX). Used to back-fill VXN before it exists (QQQ history
/// starts 1999-03); the dot-com era had Nasdaq vol far above S&P vol.
pub const VXN_VIX_RATIO_EARLY: f64 = 2.13;
/// Median VXN/VIX over the CBOE VXN file period 2009-09-14 .. 2026-10-20.
pub const VXN_VIX_RATIO_FULL: f64 = 1.17;
/// Median VXN/VIX over 2016-01-01 .. 2026-10-20.
pub const VXN_VIX_RATIO_RECENT: f64 = 1.24;

/// Scale a VIX series into a VXN proxy (`VXN ≈ ratio × VIX`).
pub fn proxy_vxn_from_vix(vix: &ScalarSeries, ratio: f64) -> ScalarSeries {
    ScalarSeries::new("VXN (VIX proxy)", vix.points.iter().map(|(d, v)| (*d, v * ratio)).collect())
}

/// Median of `num/den` on common dates within `[start, end]`.
pub fn estimate_ratio(num: &ScalarSeries, den: &ScalarSeries, start: NaiveDate, end: NaiveDate) -> Option<f64> {
    let dm: HashMap<NaiveDate, f64> = den.points.iter().copied().collect();
    let mut r: Vec<f64> = num
        .points
        .iter()
        .filter(|(d, _)| *d >= start && *d <= end)
        .filter_map(|(d, v)| dm.get(d).filter(|x| **x > 0.0).map(|x| v / x))
        .collect();
    if r.is_empty() {
        return None;
    }
    r.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = r.len();
    Some(if n % 2 == 1 { r[n / 2] } else { 0.5 * (r[n / 2 - 1] + r[n / 2]) })
}

// ------------------------------------------------------------------ parsing

/// 10-year files: `date,open,high,low,close,volume,div,split`. Prices are already
/// split-adjusted; `close` is NOT dividend-adjusted, so the adjusted close is
/// rebuilt Yahoo-style: every bar before an ex-date is multiplied by `1 - div/prev_close`.
fn parse_ten_year(text: &str, symbol: &str) -> PriceSeries {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(text.as_bytes());
    let mut rows: Vec<(Bar, f64)> = Vec::new();
    for rec in rdr.records().flatten() {
        let n = |i: usize| rec.get(i).and_then(crate::util::parse_num);
        let (Some(date), Some(close)) = (rec.get(0).and_then(crate::util::parse_date), n(4)) else { continue };
        if close <= 0.0 {
            continue;
        }
        rows.push((
            Bar {
                date,
                open: n(1).unwrap_or(close),
                high: n(2).unwrap_or(close),
                low: n(3).unwrap_or(close),
                close,
                adj_close: close,
                volume: n(5).unwrap_or(0.0),
            },
            n(6).unwrap_or(0.0),
        ));
    }
    rows.sort_by_key(|r| r.0.date);
    let mut cum = 1.0;
    for i in (0..rows.len()).rev() {
        rows[i].0.adj_close = rows[i].0.close * cum;
        let div = rows[i].1;
        if div > 0.0 && i > 0 {
            let prev = rows[i - 1].0.close;
            if prev > div {
                cum *= 1.0 - div / prev;
            }
        }
    }
    PriceSeries::new(symbol, rows.into_iter().map(|r| r.0).collect())
}

/// Splice an adjusted-close-only long history in front of a full OHLC series.
///
/// Before the seam (first date of `recent`) bars are synthetic: `adj_close` is the
/// long series scaled to join `recent.adj_close`, and open=high=low=close is the
/// long series scaled to join `recent.close` (so both columns are continuous at
/// the seam). Volume is 0 there.
pub fn splice_adjusted_history(long: &[(NaiveDate, f64)], recent: &PriceSeries) -> PriceSeries {
    let Some(first) = recent.bars.first() else {
        return recent.clone();
    };
    let seam = first.date;
    // Long-series value on the seam date (or the last one before it).
    let Some(&(_, lv)) = long.iter().rev().find(|(d, v)| *d <= seam && *v > 0.0) else {
        return recent.clone();
    };
    let (k_adj, k_close) = (first.adj_close / lv, first.close / lv);
    let mut bars: Vec<Bar> = long
        .iter()
        .filter(|(d, v)| *d < seam && *v > 0.0)
        .map(|(d, v)| {
            let c = v * k_close;
            Bar { date: *d, open: c, high: c, low: c, close: c, adj_close: v * k_adj, volume: 0.0 }
        })
        .collect();
    bars.extend(recent.bars.iter().copied());
    PriceSeries::new(recent.symbol.clone(), bars)
}

/// Merge scalar series: earlier arguments win on common dates.
pub fn merge_scalar(name: &str, parts: &[&ScalarSeries]) -> ScalarSeries {
    let mut m: BTreeMap<NaiveDate, f64> = BTreeMap::new();
    for p in parts.iter().rev() {
        for (d, v) in &p.points {
            m.insert(*d, *v);
        }
    }
    ScalarSeries::new(name, m.into_iter().collect())
}

fn scalar(text: &str, name: &str, col: Option<&str>) -> ScalarSeries {
    parse_scalar_csv(text, name, col).unwrap_or_else(|_| ScalarSeries::new(name, vec![]))
}

fn build_prices(symbol: &str) -> Option<PriceSeries> {
    match symbol {
        "QQQ" => {
            let recent = parse_ten_year(TEN_YEAR[0].1, "QQQ");
            let long = scalar(QQQ_ADJ_1999, "QQQ", Some("adj_close"));
            Some(splice_adjusted_history(&long.points, &recent))
        }
        "SPY" => {
            let recent = parse_price_csv(SPY_2019, "SPY").ok()?;
            let long = scalar(VIX_SPY_1993, "SPY", Some("SPY"));
            Some(splice_adjusted_history(&long.points, &recent))
        }
        s => TEN_YEAR.iter().find(|t| t.0 == s).map(|t| parse_ten_year(t.1, t.0)),
    }
}

fn price_cache() -> &'static Mutex<HashMap<String, Option<PriceSeries>>> {
    static C: OnceLock<Mutex<HashMap<String, Option<PriceSeries>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn norm(sym: &str) -> String {
    crate::providers::clean_symbol(sym)
}

/// Symbols with bundled daily prices.
pub fn price_symbols() -> Vec<&'static str> {
    let mut v = vec!["SPY"];
    v.extend(TEN_YEAR.iter().map(|t| t.0));
    v
}

/// Bundled daily prices for `symbol` (case-insensitive), or `None`.
///
/// * `QQQ`: 1999-03-10.. (adjusted-only before 2016-11-03, full OHLC after).
/// * `SPY`: 1993-01-29.. (adjusted-only before 2019-01-02, full OHLC after).
/// * Others: 2016-11-03.. full OHLC with rebuilt dividend-adjusted close.
pub fn prices(symbol: &str) -> Option<PriceSeries> {
    let s = norm(symbol);
    let mut c = price_cache().lock().unwrap();
    c.entry(s.clone()).or_insert_with(|| build_prices(&s)).clone()
}

/// Bundled volatility index: `"VIX"`, `"VXN"` or `"VIX3M"` (prefixes `^`/`_` ok).
///
/// * VIX: CBOE `VIX_History.csv` close (1990-01-02..), gaps/recency filled from the
///   VIX column of `VIX_SPY_1993.csv` and `VIX_close_recent.csv` (Yahoo closes).
/// * VXN: CBOE `VXN_History.csv` (2009-09-14..), with Barchart `$VXN` closes filling
///   2000-10-10..2009 and recent days.
/// * VIX3M: CBOE `VIX3M_History.csv` (2009-09-18..).
pub fn vol_index(name: &str) -> Option<ScalarSeries> {
    static VIX: OnceLock<ScalarSeries> = OnceLock::new();
    static VXN: OnceLock<ScalarSeries> = OnceLock::new();
    static VIX3M: OnceLock<ScalarSeries> = OnceLock::new();
    let s = match norm(name).as_str() {
        "VIX" => VIX.get_or_init(|| {
            let a = scalar(VIX_HISTORY, "VIX", Some("CLOSE"));
            let b = scalar(VIX_SPY_1993, "VIX", Some("VIX"));
            let c = scalar(VIX_CLOSE_RECENT, "VIX", Some("VIX"));
            merge_scalar("VIX", &[&a, &b, &c])
        }),
        "VXN" => VXN.get_or_init(|| {
            let a = scalar(VXN_HISTORY, "VXN", Some("CLOSE"));
            let b = scalar(VXN_BARCHART, "VXN", Some("Last"));
            merge_scalar("VXN", &[&a, &b])
        }),
        "VIX3M" | "VXV" => VIX3M.get_or_init(|| scalar(VIX3M_HISTORY, "VIX3M", Some("CLOSE"))),
        _ => return None,
    };
    (!s.is_empty()).then(|| s.clone())
}

/// Bundled 3-month Treasury constant-maturity yield (FRED DGS3MO), in percent.
pub fn rate(series: &str) -> Option<ScalarSeries> {
    static R: OnceLock<ScalarSeries> = OnceLock::new();
    match norm(series).as_str() {
        "DGS3MO" | "TBILL3M" | "IRX" => Some(R.get_or_init(|| scalar(DGS3MO, "DGS3MO", Some("DGS3MO"))).clone()),
        _ => None,
    }
}

/// Underlyings for which the Nasdaq-100 vol index (VXN) is the natural benchmark.
const VXN_FAMILY: &[&str] = &[
    "QQQ", "QQQM", "TQQQ", "QLD", "NDX", "XND", "NDXP", "NVDA", "MSFT", "GOOG", "GOOGL", "AMZN", "META", "AVGO", "TSM",
    "SOXX", "SMH", "SOXL", "AAPL", "TSLA", "AMD", "NFLX", "XLK", "VGT", "ONEQ",
];

/// `"VXN"` for Nasdaq/tech underlyings, `"VIX"` otherwise (SPY, VTI, SPX, ...).
pub fn default_vol_index_name(symbol: &str) -> &'static str {
    if VXN_FAMILY.contains(&norm(symbol).as_str()) {
        "VXN"
    } else {
        "VIX"
    }
}

/// VXN completed with a VIX-based proxy: dates before VXN starts use
/// `VIX × VXN_VIX_RATIO_EARLY`; gaps of more than 7 calendar days inside the VXN
/// record are filled with VIX × (median ratio of the 20 VXN observations before the gap).
pub fn vxn_with_proxy(vxn: &ScalarSeries, vix: &ScalarSeries) -> ScalarSeries {
    let Some(&(first, _)) = vxn.points.first() else {
        return proxy_vxn_from_vix(vix, VXN_VIX_RATIO_EARLY);
    };
    let vix_map: HashMap<NaiveDate, f64> = vix.points.iter().copied().collect();
    let mut out: Vec<(NaiveDate, f64)> = vix
        .points
        .iter()
        .filter(|(d, _)| *d < first)
        .map(|(d, v)| (*d, v * VXN_VIX_RATIO_EARLY))
        .collect();
    for w in 0..vxn.points.len() {
        out.push(vxn.points[w]);
        if w + 1 < vxn.points.len() {
            let (d0, d1) = (vxn.points[w].0, vxn.points[w + 1].0);
            if (d1 - d0).num_days() > 7 {
                let lo = w.saturating_sub(19);
                let mut rs: Vec<f64> = vxn.points[lo..=w]
                    .iter()
                    .filter_map(|(d, v)| vix_map.get(d).filter(|x| **x > 0.0).map(|x| v / x))
                    .collect();
                rs.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let ratio = rs.get(rs.len() / 2).copied().unwrap_or(VXN_VIX_RATIO_FULL);
                out.extend(
                    vix.points.iter().filter(|(d, _)| *d > d0 && *d < d1).map(|(d, v)| (*d, v * ratio)),
                );
            }
        }
    }
    ScalarSeries::new("VXN", out)
}

/// Default bundled vol-index series for an underlying: VXN (with VIX proxy
/// back-fill, see [`vxn_with_proxy`]) for Nasdaq/tech names, VIX otherwise.
pub fn default_vol_index_for(symbol: &str) -> Option<ScalarSeries> {
    match default_vol_index_name(symbol) {
        "VXN" => Some(vxn_with_proxy(&vol_index("VXN")?, &vol_index("VIX")?)),
        _ => vol_index("VIX"),
    }
}

/// Catalogue of everything bundled.
pub fn list() -> Vec<BundledInfo> {
    static L: OnceLock<Vec<BundledInfo>> = OnceLock::new();
    L.get_or_init(|| {
        let mut v = Vec::new();
        let mut push_p = |sym: &str, desc: &str, src: &str| {
            if let Some(p) = prices(sym) {
                if let (Some(f), Some(l)) = (p.first_date(), p.last_date()) {
                    v.push(BundledInfo {
                        symbol: sym.to_string(),
                        kind: BundledKind::Prices,
                        description: desc.to_string(),
                        first: f,
                        last: l,
                        source: src.to_string(),
                    });
                }
            }
        };
        push_p(
            "SPY",
            "SPDR S&P 500 ETF (adjusted close 1993-2018 spliced with OHLC 2019+)",
            "https://github.com/vivek-v-rao/Conditional-Skew + https://github.com/fja05680/brownbear (+ B4L1/daily-notes, mano000may/Hackathon-Project for 2026-08-31..)",
        );
        push_p(
            "QQQ",
            "Invesco QQQ Trust (adjusted close 1999-2016 spliced with OHLC 2016+)",
            "https://github.com/iqueipopg/backtest-overfitting + https://github.com/yennanliu/finance_data",
        );
        for t in TEN_YEAR.iter().skip(1) {
            push_p(t.0, t.2, SRC_YENNANLIU);
        }
        let mut push_s = |s: Option<ScalarSeries>, kind: BundledKind, desc: &str, src: &str| {
            if let Some(s) = s {
                if let (Some(f), Some(l)) = (s.points.first(), s.points.last()) {
                    v.push(BundledInfo {
                        symbol: s.name.clone(),
                        kind,
                        description: desc.to_string(),
                        first: f.0,
                        last: l.0,
                        source: src.to_string(),
                    });
                }
            }
        };
        push_s(
            vol_index("VIX"),
            BundledKind::VolIndex,
            "CBOE Volatility Index (S&P 500 30-day IV), close",
            "https://github.com/sabsab0808-IA/trade-data (CBOE VIX_History.csv) + vivek-v-rao/Conditional-Skew + KKOO245/OPTION-ALERT",
        );
        push_s(
            vol_index("VXN"),
            BundledKind::VolIndex,
            "CBOE Nasdaq-100 Volatility Index, close (pre-2009 from Barchart)",
            "https://github.com/rellis3/MacroFXModel (CBOE VXN_History.csv) + https://github.com/smallfishmacro-Git/market-dashboard (Barchart $VXN)",
        );
        push_s(
            vol_index("VIX3M"),
            BundledKind::VolIndex,
            "CBOE S&P 500 3-Month Volatility Index, close",
            "https://github.com/sabsab0808-IA/trade-data (CBOE VIX3M_History.csv)",
        );
        push_s(
            rate("DGS3MO"),
            BundledKind::Rate,
            "3-Month Treasury constant maturity yield, percent (FRED DGS3MO)",
            "https://github.com/achl0902-web/back-testing (FRED DGS3MO mirror)",
        );
        v
    })
    .clone()
}
