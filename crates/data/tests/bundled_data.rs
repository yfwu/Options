//! Integrity tests of the bundled historical data.

use chrono::NaiveDate;
use optlab_core::{PriceSeries, ScalarSeries};
use optlab_data::bundled;

fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, dd).unwrap()
}

fn check_prices(s: &PriceSeries) {
    assert!(!s.is_empty(), "{} empty", s.symbol);
    for w in s.bars.windows(2) {
        assert!(w[0].date < w[1].date, "{} not ascending at {}", s.symbol, w[1].date);
    }
    for b in &s.bars {
        for v in [b.open, b.high, b.low, b.close, b.adj_close, b.volume] {
            assert!(v.is_finite(), "{} NaN at {}", s.symbol, b.date);
        }
        assert!(b.close > 0.0 && b.adj_close > 0.0, "{} non-positive at {}", s.symbol, b.date);
        assert!(b.high + 1e-6 >= b.low, "{} high<low at {}", s.symbol, b.date);
    }
    // No absurd daily moves (bad splice / unadjusted split): |r| < 40%.
    for (i, r) in s.returns().iter().enumerate() {
        assert!(r.abs() < 0.4, "{} return {r} at {}", s.symbol, s.bars[i + 1].date);
    }
}

fn check_scalar(s: &ScalarSeries) {
    assert!(!s.is_empty());
    for w in s.points.windows(2) {
        assert!(w[0].0 < w[1].0, "{} not ascending at {}", s.name, w[1].0);
    }
    assert!(s.points.iter().all(|p| p.1.is_finite()));
}

fn bar(s: &PriceSeries, date: NaiveDate) -> optlab_core::Bar {
    *s.bars.iter().find(|b| b.date == date).unwrap_or_else(|| panic!("{} missing {date}", s.symbol))
}

#[test]
fn all_price_series_valid() {
    for sym in bundled::price_symbols() {
        let s = bundled::prices(sym).unwrap();
        check_prices(&s);
        assert_eq!(s.last_date(), Some(d(2026, 11, 3)), "{sym} last date");
    }
    assert!(bundled::prices("NOPE").is_none());
    assert!(bundled::prices("qqq").is_some());
}

#[test]
fn qqq_splice() {
    let s = bundled::prices("QQQ").unwrap();
    assert_eq!(s.first_date(), Some(d(1999, 3, 10)));
    let seam = d(2016, 11, 3);
    let i = s.bars.iter().position(|b| b.date == seam).unwrap();
    let (a, b) = (s.bars[i - 1], s.bars[i]);
    assert!((b.adj_close / a.adj_close - 1.0).abs() < 0.01, "adj jump at seam");
    assert!((b.close / a.close - 1.0).abs() < 0.01, "close jump at seam");
    // OHLC from the 10y file after the seam (unadjusted close 2020-03-23 = 170.46).
    let c = bar(&s, d(2020, 3, 23));
    assert!((c.close - 170.46).abs() < 1e-9);
    assert!(c.volume > 0.0);
    // The rebuilt dividend-adjusted close tracks the independent QQQ_adj_1999 file:
    // ratio constant over 2016-11..2026-09 within 0.5%.
    let long = optlab_data::csv_import::parse_scalar_csv(
        include_str!("../../../data/QQQ_adj_1999.csv"),
        "QQQ",
        Some("adj_close"),
    )
    .unwrap();
    let ratios: Vec<f64> = long
        .points
        .iter()
        .filter(|(dt, _)| *dt >= seam && *dt <= d(2026, 9, 18))
        .filter_map(|(dt, v)| s.bars.iter().find(|b| b.date == *dt).map(|b| b.adj_close / v))
        .collect();
    assert!(ratios.len() > 2400);
    let (mn, mx) = ratios.iter().fold((f64::MAX, f64::MIN), |(a, b), r| (a.min(*r), b.max(*r)));
    assert!(mx / mn - 1.0 < 0.005, "ratio drift {mn}..{mx}");
}

#[test]
fn spy_splice() {
    let s = bundled::prices("SPY").unwrap();
    assert_eq!(s.first_date(), Some(d(1993, 1, 29)));
    let seam = d(2019, 1, 2);
    let i = s.bars.iter().position(|b| b.date == seam).unwrap();
    let (a, b) = (s.bars[i - 1], s.bars[i]);
    assert!((b.adj_close / a.adj_close - 1.0).abs() < 0.02, "adj jump at seam"); // 2019-01-02 was +0.1%
    assert!((b.close / a.close - 1.0).abs() < 0.02);
    // Known unadjusted close 2020-03-23 = 222.95; COVID low.
    assert!((bar(&s, d(2020, 3, 23)).close - 222.95).abs() < 0.01);
    // Extension rows (2026-08-31..) from the newer mirrors.
    let last = bar(&s, d(2026, 11, 3));
    assert!((last.close - 754.35).abs() < 0.01);
    assert!((last.adj_close - 754.35).abs() < 0.01);
    // Total return 1993-2026 should be large (> 20x) — catches a broken splice scale.
    assert!(s.bars.last().unwrap().adj_close / s.bars[0].adj_close > 20.0);
}

#[test]
fn ten_year_dividend_adjustment() {
    let s = bundled::prices("VTI").unwrap();
    assert_eq!(s.first_date(), Some(d(2016, 11, 3)));
    let first = s.bars[0];
    let last = *s.bars.last().unwrap();
    assert!(first.adj_close < first.close * 0.9, "VTI 10y dividends should shrink early adj closes");
    assert!((last.adj_close - last.close).abs() < 1e-9);
    // NVDA: already split adjusted (no 4:1 / 10:1 jumps), tiny dividends.
    let n = bundled::prices("NVDA").unwrap();
    let r = bar(&n, d(2024, 6, 10)).close / bar(&n, d(2024, 6, 7)).close;
    assert!((r - 1.0).abs() < 0.1);
}

#[test]
fn vix_values() {
    let v = bundled::vol_index("VIX").unwrap();
    check_scalar(&v);
    assert_eq!(v.points[0].0, d(1990, 1, 2));
    assert_eq!(v.value_at(d(2020, 3, 16)), Some(82.69));
    assert_eq!(v.value_at(d(2008, 11, 20)), Some(80.86));
    assert_eq!(v.last().unwrap().0, d(2026, 11, 3));
    assert!(v.points.iter().all(|p| p.1 > 5.0 && p.1 < 100.0));
    assert!(bundled::vol_index("^vix").is_some());
    let v3 = bundled::vol_index("VIX3M").unwrap();
    check_scalar(&v3);
    assert_eq!(v3.value_at(d(2020, 3, 16)), Some(67.7));
}

#[test]
fn vxn_and_proxy() {
    let x = bundled::vol_index("VXN").unwrap();
    check_scalar(&x);
    assert_eq!(x.points[0].0, d(2000, 10, 10));
    assert_eq!(x.last().unwrap().0, d(2026, 11, 3));
    assert_eq!(x.value_at(d(2026, 10, 20)), Some(25.22)); // CBOE file wins on overlap
    let vix = bundled::vol_index("VIX").unwrap();
    let early = bundled::estimate_ratio(&x, &vix, d(2000, 10, 10), d(2002, 12, 31)).unwrap();
    assert!((early - bundled::VXN_VIX_RATIO_EARLY).abs() < 0.02, "{early}");
    let full = bundled::estimate_ratio(&x, &vix, d(2009, 9, 14), d(2026, 10, 20)).unwrap();
    assert!((full - bundled::VXN_VIX_RATIO_FULL).abs() < 0.02, "{full}");
    let recent = bundled::estimate_ratio(&x, &vix, d(2016, 1, 1), d(2026, 10, 20)).unwrap();
    assert!((recent - bundled::VXN_VIX_RATIO_RECENT).abs() < 0.02, "{recent}");

    let q = bundled::default_vol_index_for("QQQ").unwrap();
    check_scalar(&q);
    assert_eq!(bundled::default_vol_index_name("qqq"), "VXN");
    assert_eq!(bundled::default_vol_index_name("SPY"), "VIX");
    // Covers the whole QQQ history.
    assert!(q.points[0].0 <= d(1999, 3, 10));
    let pre = q.value_at(d(1999, 6, 1)).unwrap();
    let vix_pre = vix.value_at(d(1999, 6, 1)).unwrap();
    assert!((pre - vix_pre * bundled::VXN_VIX_RATIO_EARLY).abs() < 1e-9);
    // Gap 2000-11-06..2001-01-23 in the Barchart record is proxy-filled.
    assert!(q.points.iter().any(|p| p.0 == d(2000, 12, 15)));
    assert_eq!(bundled::default_vol_index_for("SPY").unwrap().len(), vix.len());
}

#[test]
fn rates_and_catalogue() {
    let r = bundled::rate("DGS3MO").unwrap();
    check_scalar(&r);
    assert_eq!(r.value_at(d(2020, 4, 1)), Some(0.09));
    assert_eq!(r.value_at(d(2023, 7, 31)), Some(5.55));
    assert_eq!(r.last().unwrap().0, d(2026, 11, 2));
    assert!(r.points.iter().all(|p| p.1 >= 0.0 && p.1 < 20.0));

    let l = bundled::list();
    assert_eq!(l.len(), bundled::price_symbols().len() + 4);
    let q = l.iter().find(|i| i.symbol == "QQQ").unwrap();
    assert_eq!(q.first, d(1999, 3, 10));
    assert!(l.iter().all(|i| i.first < i.last && !i.source.is_empty()));
}
