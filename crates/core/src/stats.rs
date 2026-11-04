//! Performance statistics for equity curves.
//!
//! Conventions: returns and drawdowns are decimals (0.12 = 12%); `max_drawdown`
//! is **negative** (−0.35 = −35%). The periods-per-year used for annualising
//! volatility/Sharpe is inferred from the curve (`n_returns / years`), so the
//! functions work for daily, weekly or monthly curves.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

/// Summary statistics of one equity curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PerformanceStats {
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// Calendar years between `start` and `end` (ACT/365.25).
    pub years: f64,
    pub n_obs: usize,
    pub initial_value: f64,
    pub final_value: f64,
    pub total_return: f64,
    /// Compound annual growth rate; −1 when the curve hits zero.
    pub cagr: f64,
    /// Arithmetic mean period return × periods per year.
    pub ann_return: f64,
    pub ann_vol: f64,
    pub sharpe: f64,
    pub sortino: f64,
    /// Maximum drawdown (negative).
    pub max_drawdown: f64,
    /// Peak date before the max drawdown.
    pub max_dd_start: NaiveDate,
    pub max_dd_trough: NaiveDate,
    /// First date the prior peak was regained (None if never).
    pub max_dd_recovery: Option<NaiveDate>,
    /// Longest underwater stretch in calendar days (peak → recovery or end).
    pub longest_dd_days: i64,
    /// CAGR / |max drawdown|.
    pub calmar: f64,
    /// Ulcer index: RMS of percentage drawdowns (decimal).
    pub ulcer_index: f64,
    pub best_year: Option<(i32, f64)>,
    pub worst_year: Option<(i32, f64)>,
    /// Calendar-year returns (first/last year may be partial).
    pub yearly: Vec<(i32, f64)>,
    pub pct_positive_months: f64,
    pub best_month: f64,
    pub worst_month: f64,
    pub best_day: f64,
    pub worst_day: f64,
    /// 95% one-period historical value at risk (positive number = loss).
    pub var95: f64,
    /// Expected shortfall beyond VaR95 (positive number = loss).
    pub cvar95: f64,
    pub periods_per_year: f64,
}

fn period_returns(curve: &[(NaiveDate, f64)]) -> Vec<f64> {
    curve
        .windows(2)
        .map(|w| if w[0].1 > 0.0 { w[1].1 / w[0].1 - 1.0 } else { 0.0 })
        .collect()
}

/// Drawdown from running peak at each point (≤ 0).
pub fn drawdown_series(curve: &[(NaiveDate, f64)]) -> Vec<(NaiveDate, f64)> {
    let mut peak = f64::NEG_INFINITY;
    curve
        .iter()
        .map(|&(d, v)| {
            peak = peak.max(v);
            let dd = if peak > 0.0 { v / peak - 1.0 } else { 0.0 };
            (d, dd)
        })
        .collect()
}

/// Calendar-year returns. The base of each year is the last value of the previous
/// year (or the first value of the curve for the first year).
pub fn yearly_returns(curve: &[(NaiveDate, f64)]) -> Vec<(i32, f64)> {
    let mut out = Vec::new();
    if curve.is_empty() {
        return out;
    }
    let mut base = curve[0].1;
    let mut year = curve[0].0.year();
    let mut last = curve[0].1;
    for &(d, v) in curve {
        if d.year() != year {
            out.push((year, if base > 0.0 { last / base - 1.0 } else { 0.0 }));
            base = last;
            year = d.year();
        }
        last = v;
    }
    out.push((year, if base > 0.0 { last / base - 1.0 } else { 0.0 }));
    out
}

/// Calendar-month returns as (year, month, return).
pub fn monthly_returns(curve: &[(NaiveDate, f64)]) -> Vec<(i32, u32, f64)> {
    let mut out = Vec::new();
    if curve.is_empty() {
        return out;
    }
    let mut base = curve[0].1;
    let mut key = (curve[0].0.year(), curve[0].0.month());
    let mut last = curve[0].1;
    for &(d, v) in curve {
        let k = (d.year(), d.month());
        if k != key {
            out.push((key.0, key.1, if base > 0.0 { last / base - 1.0 } else { 0.0 }));
            base = last;
            key = k;
        }
        last = v;
    }
    out.push((key.0, key.1, if base > 0.0 { last / base - 1.0 } else { 0.0 }));
    out
}

/// Rolling returns over `window` observations, dated at the window end.
/// When `annualize` the return is converted to a CAGR using calendar time.
pub fn rolling_returns(curve: &[(NaiveDate, f64)], window: usize, annualize: bool) -> Vec<(NaiveDate, f64)> {
    if window == 0 || curve.len() <= window {
        return Vec::new();
    }
    (window..curve.len())
        .map(|i| {
            let (d0, v0) = curve[i - window];
            let (d1, v1) = curve[i];
            let tr = if v0 > 0.0 { v1 / v0 - 1.0 } else { 0.0 };
            let r = if annualize {
                let yrs = (d1 - d0).num_days() as f64 / 365.25;
                if yrs > 0.0 && 1.0 + tr > 0.0 {
                    (1.0 + tr).powf(1.0 / yrs) - 1.0
                } else if yrs > 0.0 {
                    -1.0
                } else {
                    tr
                }
            } else {
                tr
            };
            (d1, r)
        })
        .collect()
}

/// Rolling CAGR over a calendar window of `years` (uses ≈252 obs/year spacing
/// inferred from the curve).
pub fn rolling_cagr(curve: &[(NaiveDate, f64)], years: f64) -> Vec<(NaiveDate, f64)> {
    if curve.len() < 2 {
        return Vec::new();
    }
    let span = (curve[curve.len() - 1].0 - curve[0].0).num_days() as f64 / 365.25;
    if span <= 0.0 {
        return Vec::new();
    }
    let ppy = (curve.len() - 1) as f64 / span;
    rolling_returns(curve, (years * ppy).round().max(1.0) as usize, true)
}

/// Rescale a curve so it starts at `start_value`.
pub fn normalize_curve(curve: &[(NaiveDate, f64)], start_value: f64) -> Vec<(NaiveDate, f64)> {
    match curve.first() {
        Some(&(_, v0)) if v0 > 0.0 => curve.iter().map(|&(d, v)| (d, v / v0 * start_value)).collect(),
        _ => curve.to_vec(),
    }
}

fn quantile_sorted(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let pos = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let w = pos - lo as f64;
    sorted[lo] * (1.0 - w) + sorted[hi] * w
}

/// Linear-interpolated quantile (q in 0..1) of unsorted data; NaN if empty.
pub fn quantile(data: &[f64], q: f64) -> f64 {
    let mut v: Vec<f64> = data.iter().copied().filter(|x| x.is_finite()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    quantile_sorted(&v, q)
}

/// Compute summary statistics of an equity curve (dates ascending).
/// `rf_annual` is the risk-free rate used for Sharpe/Sortino (decimal).
pub fn compute_stats(curve: &[(NaiveDate, f64)], rf_annual: f64) -> PerformanceStats {
    let mut s = PerformanceStats { n_obs: curve.len(), ..Default::default() };
    if curve.is_empty() {
        return s;
    }
    let (start, v0) = curve[0];
    let (end, v1) = curve[curve.len() - 1];
    s.start = start;
    s.end = end;
    s.initial_value = v0;
    s.final_value = v1;
    s.years = (end - start).num_days() as f64 / 365.25;
    s.max_dd_start = start;
    s.max_dd_trough = start;
    if curve.len() < 2 || v0 <= 0.0 {
        return s;
    }
    s.total_return = v1 / v0 - 1.0;
    s.cagr = if v1 <= 0.0 {
        -1.0
    } else if s.years > 0.0 {
        (v1 / v0).powf(1.0 / s.years) - 1.0
    } else {
        s.total_return
    };

    let rets = period_returns(curve);
    let n = rets.len() as f64;
    let ppy = if s.years > 0.05 { n / s.years } else { 252.0 };
    s.periods_per_year = ppy;
    let mean = rets.iter().sum::<f64>() / n;
    let var = if rets.len() > 1 { rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0) } else { 0.0 };
    let sd = var.sqrt();
    s.ann_return = mean * ppy;
    s.ann_vol = sd * ppy.sqrt();
    let rf_p = rf_annual / ppy;
    let ex_mean = mean - rf_p;
    s.sharpe = if sd > 0.0 { ex_mean / sd * ppy.sqrt() } else { 0.0 };
    let dd2 = rets.iter().map(|r| (r - rf_p).min(0.0).powi(2)).sum::<f64>() / n;
    s.sortino = if dd2 > 0.0 { ex_mean * ppy / (dd2.sqrt() * ppy.sqrt()) } else { 0.0 };
    s.best_day = rets.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    s.worst_day = rets.iter().copied().fold(f64::INFINITY, f64::min);
    let mut sorted = rets.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let var95 = quantile_sorted(&sorted, 0.05);
    s.var95 = -var95;
    let tail: Vec<f64> = sorted.iter().copied().filter(|&r| r <= var95).collect();
    s.cvar95 = if tail.is_empty() { s.var95 } else { -tail.iter().sum::<f64>() / tail.len() as f64 };

    // Drawdowns
    let mut peak = v0;
    let mut peak_date = start;
    let mut max_dd = 0.0;
    let mut mdd_peak = start;
    let mut mdd_trough = start;
    let mut sum_dd2 = 0.0;
    let mut longest = 0i64;
    let mut underwater_since: Option<NaiveDate> = None;
    for &(d, v) in curve {
        if v >= peak {
            if let Some(u) = underwater_since.take() {
                longest = longest.max((d - u).num_days());
            }
            peak = v;
            peak_date = d;
        } else {
            if underwater_since.is_none() {
                underwater_since = Some(peak_date);
            }
            let dd = if peak > 0.0 { v / peak - 1.0 } else { 0.0 };
            sum_dd2 += dd * dd;
            if dd < max_dd {
                max_dd = dd;
                mdd_peak = peak_date;
                mdd_trough = d;
            }
        }
    }
    if let Some(u) = underwater_since {
        longest = longest.max((end - u).num_days());
    }
    s.max_drawdown = max_dd;
    s.max_dd_start = mdd_peak;
    s.max_dd_trough = mdd_trough;
    if max_dd < 0.0 {
        let peak_val = curve.iter().find(|p| p.0 == mdd_peak).map(|p| p.1).unwrap_or(v0);
        s.max_dd_recovery = curve.iter().find(|p| p.0 > mdd_trough && p.1 >= peak_val).map(|p| p.0);
    }
    s.longest_dd_days = longest;
    s.ulcer_index = (sum_dd2 / curve.len() as f64).sqrt();
    s.calmar = if max_dd < 0.0 { s.cagr / max_dd.abs() } else { 0.0 };

    s.yearly = yearly_returns(curve);
    let cmp = |a: &&(i32, f64), b: &&(i32, f64)| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
    s.best_year = s.yearly.iter().max_by(cmp).copied();
    s.worst_year = s.yearly.iter().min_by(cmp).copied();
    let months = monthly_returns(curve);
    if !months.is_empty() {
        s.pct_positive_months = months.iter().filter(|m| m.2 > 0.0).count() as f64 / months.len() as f64;
        s.best_month = months.iter().map(|m| m.2).fold(f64::NEG_INFINITY, f64::max);
        s.worst_month = months.iter().map(|m| m.2).fold(f64::INFINITY, f64::min);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    fn geometric_curve(start: NaiveDate, days: i64, total_mult: f64) -> Vec<(NaiveDate, f64)> {
        (0..=days)
            .map(|i| (start + chrono::Duration::days(i), 100.0 * total_mult.powf(i as f64 / days as f64)))
            .collect()
    }

    #[test]
    fn cagr_doubling() {
        // 2021 is not a leap year: 365 days ⇒ years = 365/365.25
        let c = geometric_curve(d(2021, 1, 1), 365, 2.0);
        let s = compute_stats(&c, 0.0);
        assert!((s.total_return - 1.0).abs() < 1e-12);
        let expected = 2f64.powf(365.25 / 365.0) - 1.0;
        assert!((s.cagr - expected).abs() < 1e-9, "{}", s.cagr);
        assert_eq!(s.max_drawdown, 0.0);
        assert!(s.ann_vol < 1e-9);
        assert_eq!(s.pct_positive_months, 1.0);
    }

    #[test]
    fn known_drawdown() {
        let vals = [100.0, 120.0, 90.0, 60.0, 80.0, 125.0, 110.0];
        let c: Vec<_> = vals.iter().enumerate().map(|(i, &v)| (d(2020, 1, 1) + chrono::Duration::days(i as i64 * 10), v)).collect();
        let s = compute_stats(&c, 0.0);
        assert!((s.max_drawdown - (-0.5)).abs() < 1e-12);
        assert_eq!(s.max_dd_start, c[1].0);
        assert_eq!(s.max_dd_trough, c[3].0);
        assert_eq!(s.max_dd_recovery, Some(c[5].0));
        assert_eq!(s.longest_dd_days, 40);
        let dds = drawdown_series(&c);
        assert!((dds[3].1 + 0.5).abs() < 1e-12);
        assert!((dds[6].1 - (110.0 / 125.0 - 1.0)).abs() < 1e-12);
    }

    #[test]
    fn yearly_and_wipeout() {
        let c = vec![(d(2020, 6, 1), 100.0), (d(2020, 12, 31), 110.0), (d(2021, 6, 1), 121.0), (d(2021, 12, 31), 0.0)];
        let y = yearly_returns(&c);
        assert_eq!(y.len(), 2);
        assert!((y[0].1 - 0.1).abs() < 1e-12);
        assert!((y[1].1 + 1.0).abs() < 1e-12);
        let s = compute_stats(&c, 0.02);
        assert_eq!(s.cagr, -1.0);
        assert!((s.max_drawdown + 1.0).abs() < 1e-12);
        assert_eq!(s.worst_year.unwrap().0, 2021);
    }

    #[test]
    fn rolling_and_quantile() {
        let c = geometric_curve(d(2020, 1, 1), 730, 4.0);
        let r = rolling_returns(&c, 365, false);
        assert!((r[0].1 - 1.0).abs() < 1e-9);
        assert!((quantile(&[3.0, 1.0, 2.0], 0.5) - 2.0).abs() < 1e-12);
        assert!(rolling_cagr(&c, 1.0).len() > 300);
    }
}
