//! Monte Carlo simulation of underlying paths and strategy P&L distributions.
//!
//! * Daily steps, 252 trading days per year (`dt = 1/252`).
//! * Each path has its own `StdRng` seeded from `(seed, path index)`, and paths are
//!   generated in parallel with rayon, so results are **deterministic for a given
//!   seed** regardless of thread count.
//! * Drift parameters `mu` are annual **expected price returns** in log-continuous
//!   terms: `E[S_T] = S_0·e^{μT}` for GBM, Merton (jump-compensated) and Heston.

use crate::history::PriceSeries;
use crate::strategy::{MarketParams, Strategy};
use anyhow::{bail, Result};
use chrono::NaiveDate;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::StandardNormal;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

/// Trading days per year used for stepping.
pub const TRADING_DAYS_PER_YEAR: f64 = 252.0;

/// Stochastic model for the underlying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PathModel {
    /// Geometric Brownian motion.
    Gbm { mu: f64, sigma: f64 },
    /// Merton jump-diffusion: Poisson(λ/yr) jumps with log-size N(jump_mean, jump_std²).
    /// Drift is compensated so `E[S_T] = S_0·e^{μT}`.
    MertonJump { mu: f64, sigma: f64, lambda: f64, jump_mean: f64, jump_std: f64 },
    /// Heston stochastic volatility (full-truncation Euler). `v0`, `theta` are variances.
    Heston { mu: f64, v0: f64, kappa: f64, theta: f64, xi: f64, rho: f64 },
    /// Circular block bootstrap of historical daily log returns (keeps their drift,
    /// fat tails and short-range autocorrelation / vol clustering within blocks).
    Bootstrap { log_returns: Vec<f64>, block: usize },
}

impl PathModel {
    /// Short Traditional Chinese label for UIs.
    pub fn label_zh(&self) -> &'static str {
        match self {
            PathModel::Gbm { .. } => "幾何布朗運動 (GBM)",
            PathModel::MertonJump { .. } => "Merton 跳躍擴散",
            PathModel::Heston { .. } => "Heston 隨機波動",
            PathModel::Bootstrap { .. } => "歷史區塊拔靴法",
        }
    }

    /// Approximate annualised volatility implied by the model parameters.
    pub fn approx_vol(&self) -> f64 {
        match self {
            PathModel::Gbm { sigma, .. } => *sigma,
            PathModel::MertonJump { sigma, lambda, jump_mean, jump_std, .. } => {
                (sigma * sigma + lambda * (jump_mean * jump_mean + jump_std * jump_std)).sqrt()
            }
            PathModel::Heston { v0, theta, .. } => (0.5 * (v0 + theta)).max(0.0).sqrt(),
            PathModel::Bootstrap { log_returns, .. } => {
                let (_, s) = mean_std(log_returns);
                s * TRADING_DAYS_PER_YEAR.sqrt()
            }
        }
    }
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn path_rng(seed: u64, path: usize) -> StdRng {
    StdRng::seed_from_u64(splitmix64(seed ^ splitmix64(path as u64)))
}

/// Run one path for `days` steps, calling `emit(price)` after each step.
fn run_path(model: &PathModel, spot: f64, days: usize, rng: &mut StdRng, mut emit: impl FnMut(f64)) {
    let dt = 1.0 / TRADING_DAYS_PER_YEAR;
    let sdt = dt.sqrt();
    let mut ln_s = spot.ln();
    match model {
        PathModel::Gbm { mu, sigma } => {
            let drift = (mu - 0.5 * sigma * sigma) * dt;
            let vol = sigma * sdt;
            for _ in 0..days {
                let z: f64 = rng.sample(StandardNormal);
                ln_s += drift + vol * z;
                emit(ln_s.exp());
            }
        }
        PathModel::MertonJump { mu, sigma, lambda, jump_mean, jump_std } => {
            let k = (jump_mean + 0.5 * jump_std * jump_std).exp() - 1.0;
            let drift = (mu - 0.5 * sigma * sigma - lambda * k) * dt;
            let vol = sigma * sdt;
            let lam_dt = (lambda * dt).max(0.0);
            let p0 = (-lam_dt).exp();
            for _ in 0..days {
                let z: f64 = rng.sample(StandardNormal);
                ln_s += drift + vol * z;
                if lam_dt > 0.0 {
                    // Inverse-transform Poisson sampling (λ·dt is small).
                    let u: f64 = rng.gen();
                    let (mut n, mut p, mut cum) = (0u32, p0, p0);
                    while u > cum && n < 50 {
                        n += 1;
                        p *= lam_dt / n as f64;
                        cum += p;
                    }
                    if n > 0 {
                        let zj: f64 = rng.sample(StandardNormal);
                        let nf = n as f64;
                        ln_s += nf * jump_mean + nf.sqrt() * jump_std * zj;
                    }
                }
                emit(ln_s.exp());
            }
        }
        PathModel::Heston { mu, v0, kappa, theta, xi, rho } => {
            let rho = rho.clamp(-0.999, 0.999);
            let rho_c = (1.0 - rho * rho).sqrt();
            let mut v = *v0;
            for _ in 0..days {
                let z1: f64 = rng.sample(StandardNormal);
                let z2: f64 = rng.sample(StandardNormal);
                let vp = v.max(0.0);
                let sv = (vp * dt).sqrt();
                ln_s += (mu - 0.5 * vp) * dt + sv * (rho * z1 + rho_c * z2);
                v += kappa * (theta - vp) * dt + xi * sv * z1;
                emit(ln_s.exp());
            }
        }
        PathModel::Bootstrap { log_returns, block } => {
            let n = log_returns.len();
            let block = (*block).max(1);
            let mut idx = 0usize;
            for d in 0..days {
                if n == 0 {
                    emit(spot);
                    continue;
                }
                if d % block == 0 {
                    idx = rng.gen_range(0..n);
                } else {
                    idx = (idx + 1) % n;
                }
                ln_s += log_returns[idx];
                emit(ln_s.exp());
            }
        }
    }
}

/// Simulate `n_paths` daily paths of `days` steps. Each path has `days + 1`
/// points starting with `spot`.
pub fn simulate_paths(model: &PathModel, spot: f64, days: usize, n_paths: usize, seed: u64) -> Vec<Vec<f64>> {
    (0..n_paths)
        .into_par_iter()
        .map(|i| {
            let mut rng = path_rng(seed, i);
            let mut path = Vec::with_capacity(days + 1);
            path.push(spot);
            run_path(model, spot, days, &mut rng, |s| path.push(s));
            path
        })
        .collect()
}

/// Terminal prices only (identical to the last point of [`simulate_paths`] for the same seed).
pub fn simulate_terminal(model: &PathModel, spot: f64, days: usize, n_paths: usize, seed: u64) -> Vec<f64> {
    (0..n_paths)
        .into_par_iter()
        .map(|i| {
            let mut rng = path_rng(seed, i);
            let mut last = spot;
            run_path(model, spot, days, &mut rng, |s| last = s);
            last
        })
        .collect()
}

/// Cross-sectional quantiles of a path set at one step (for fan charts).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PathQuantiles {
    pub step: usize,
    pub p05: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p95: f64,
    pub mean: f64,
}

/// Quantile bands per step of a path set (every `stride` steps, always including the last).
pub fn path_quantiles(paths: &[Vec<f64>], stride: usize) -> Vec<PathQuantiles> {
    let len = paths.iter().map(Vec::len).min().unwrap_or(0);
    if len == 0 {
        return Vec::new();
    }
    let stride = stride.max(1);
    let mut steps: Vec<usize> = (0..len).step_by(stride).collect();
    if *steps.last().unwrap() != len - 1 {
        steps.push(len - 1);
    }
    steps
        .into_par_iter()
        .map(|step| {
            let mut col: Vec<f64> = paths.iter().map(|p| p[step]).collect();
            col.sort_by(f64::total_cmp);
            let mean = col.iter().sum::<f64>() / col.len() as f64;
            PathQuantiles {
                step,
                p05: quantile_sorted(&col, 0.05),
                p25: quantile_sorted(&col, 0.25),
                p50: quantile_sorted(&col, 0.50),
                p75: quantile_sorted(&col, 0.75),
                p95: quantile_sorted(&col, 0.95),
                mean,
            }
        })
        .collect()
}

/// Summary statistics of a sample (typically P&L in $ or terminal prices).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Distribution {
    pub n: usize,
    pub mean: f64,
    pub std: f64,
    pub median: f64,
    pub p05: f64,
    pub p25: f64,
    pub p75: f64,
    pub p95: f64,
    /// Fraction of samples > 0.
    pub prob_profit: f64,
    /// 95% Value-at-Risk as a positive loss: `max(0, −p05)`.
    pub var95: f64,
    /// 95% conditional VaR (expected shortfall) as a positive loss: `max(0, −mean(x | x ≤ p05))`.
    pub cvar95: f64,
    pub min: f64,
    pub max: f64,
    /// (bin centre, relative frequency); frequencies sum to 1.
    pub histogram: Vec<(f64, f64)>,
}

/// Linear-interpolated quantile of a sorted slice.
pub fn quantile_sorted(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return f64::NAN;
    }
    let pos = p.clamp(0.0, 1.0) * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let w = pos - lo as f64;
    sorted[lo] * (1.0 - w) + sorted[hi] * w
}

fn mean_std(x: &[f64]) -> (f64, f64) {
    let n = x.len();
    if n == 0 {
        return (f64::NAN, f64::NAN);
    }
    let m = x.iter().sum::<f64>() / n as f64;
    if n < 2 {
        return (m, 0.0);
    }
    let v = x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (n - 1) as f64;
    (m, v.sqrt())
}

/// Summarise with a 50-bin histogram.
pub fn summarize(samples: &[f64]) -> Distribution {
    summarize_with_bins(samples, 50)
}

/// Summarise with a custom number of histogram bins. Non-finite samples are ignored.
pub fn summarize_with_bins(samples: &[f64], bins: usize) -> Distribution {
    let mut s: Vec<f64> = samples.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return Distribution::default();
    }
    s.par_sort_unstable_by(f64::total_cmp);
    let n = s.len();
    let (mean, std) = mean_std(&s);
    let p05 = quantile_sorted(&s, 0.05);
    let tail: Vec<f64> = s.iter().copied().take_while(|x| *x <= p05).collect();
    let tail_mean = if tail.is_empty() { s[0] } else { tail.iter().sum::<f64>() / tail.len() as f64 };
    let (min, max) = (s[0], s[n - 1]);
    let bins = bins.max(1);
    let histogram = if max > min {
        let w = (max - min) / bins as f64;
        let mut counts = vec![0usize; bins];
        for x in &s {
            let b = (((x - min) / w) as usize).min(bins - 1);
            counts[b] += 1;
        }
        counts
            .into_iter()
            .enumerate()
            .map(|(i, c)| (min + w * (i as f64 + 0.5), c as f64 / n as f64))
            .collect()
    } else {
        vec![(min, 1.0)]
    };
    Distribution {
        n,
        mean,
        std,
        median: quantile_sorted(&s, 0.5),
        p05,
        p25: quantile_sorted(&s, 0.25),
        p75: quantile_sorted(&s, 0.75),
        p95: quantile_sorted(&s, 0.95),
        prob_profit: s.iter().filter(|x| **x > 0.0).count() as f64 / n as f64,
        var95: (-p05).max(0.0),
        cvar95: (-tail_mean).max(0.0),
        min,
        max,
        histogram,
    }
}

/// Trading days between two calendar dates (`calendar days × 252/365`, rounded).
pub fn trading_days_between(from: NaiveDate, to: NaiveDate) -> usize {
    let cal = (to - from).num_days();
    if cal <= 0 {
        0
    } else {
        ((cal as f64) * TRADING_DAYS_PER_YEAR / 365.0).round() as usize
    }
}

/// Strategy P&L ($) samples at `horizon`: simulate the underlying from `spot` on
/// `strategy.created`, then mark every leg on `horizon` with its own IV + `params.vol_shift`
/// (expired legs at intrinsic). For large path counts the strategy is valued on a fine
/// spot grid (including every strike, so expiry kinks are exact) and linearly interpolated.
pub fn strategy_pnl_samples(
    strategy: &Strategy,
    model: &PathModel,
    spot: f64,
    horizon: NaiveDate,
    params: &MarketParams,
    n_paths: usize,
    seed: u64,
) -> Vec<f64> {
    let days = trading_days_between(strategy.created, horizon);
    let terminals = simulate_terminal(model, spot, days, n_paths, seed);
    let cost = strategy.entry_cost();
    const GRID: usize = 2001;
    if terminals.len() <= GRID {
        return terminals.par_iter().map(|s| strategy.value_at(*s, horizon, params) - cost).collect();
    }
    let lo = terminals.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = terminals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if hi - lo < 1e-12 {
        let v = strategy.value_at(lo, horizon, params) - cost;
        return vec![v; terminals.len()];
    }
    let mut grid: Vec<f64> = (0..GRID).map(|i| lo + (hi - lo) * i as f64 / (GRID - 1) as f64).collect();
    grid.extend(strategy.strikes().into_iter().filter(|k| *k > lo && *k < hi));
    grid.sort_by(f64::total_cmp);
    grid.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
    let vals: Vec<f64> = if params.american {
        // Binomial trees are expensive: value the fine grid with European BS and add the
        // (smooth) early-exercise premium interpolated from a coarse American grid.
        let euro = MarketParams { american: false, ..*params };
        let coarse: Vec<f64> = (0..AMERICAN_GRID).map(|i| lo + (hi - lo) * i as f64 / (AMERICAN_GRID - 1) as f64).collect();
        let premium: Vec<f64> = coarse
            .par_iter()
            .map(|s| strategy.value_at(*s, horizon, params) - strategy.value_at(*s, horizon, &euro))
            .collect();
        grid.par_iter()
            .map(|s| strategy.value_at(*s, horizon, &euro) + interp(&coarse, &premium, *s) - cost)
            .collect()
    } else {
        grid.par_iter().map(|s| strategy.value_at(*s, horizon, params) - cost).collect()
    };
    terminals.par_iter().map(|&s| interp(&grid, &vals, s)).collect()
}

/// Number of spot nodes valued with the American (binomial) model in [`strategy_pnl_samples`].
const AMERICAN_GRID: usize = 121;

/// Piecewise-linear interpolation on a sorted grid (clamped extrapolation of the end segments).
fn interp(xs: &[f64], ys: &[f64], x: f64) -> f64 {
    if xs.len() < 2 {
        return ys.first().copied().unwrap_or(0.0);
    }
    let j = xs.partition_point(|g| *g < x).clamp(1, xs.len() - 1);
    let (x0, x1) = (xs[j - 1], xs[j]);
    let w = if x1 > x0 { ((x - x0) / (x1 - x0)).clamp(0.0, 1.0) } else { 0.0 };
    ys[j - 1] * (1.0 - w) + ys[j] * w
}

/// Distribution of strategy P&L at `horizon` (see [`strategy_pnl_samples`]).
pub fn strategy_pnl_distribution(
    strategy: &Strategy,
    model: &PathModel,
    spot: f64,
    horizon: NaiveDate,
    params: &MarketParams,
    n_paths: usize,
    seed: u64,
) -> Distribution {
    summarize(&strategy_pnl_samples(strategy, model, spot, horizon, params, n_paths, seed))
}

/// Fit GBM from daily adjusted-close log returns: `σ = sd·√252`, `μ = mean·252 + σ²/2`.
pub fn fit_gbm(series: &PriceSeries) -> Result<PathModel> {
    let lr = series.log_returns();
    if lr.len() < 2 {
        bail!("need at least 3 prices to fit GBM");
    }
    let (m, s) = mean_std(&lr);
    let sigma = s * TRADING_DAYS_PER_YEAR.sqrt();
    Ok(PathModel::Gbm { mu: m * TRADING_DAYS_PER_YEAR + 0.5 * sigma * sigma, sigma })
}

/// Fit a Merton jump-diffusion with a simple threshold estimator: daily log returns
/// further than `3 × robust σ` (MAD·1.4826) from the median are classified as jumps.
/// λ = jumps per year, jump mean/std from the jump returns, diffusion σ from the rest,
/// and μ chosen so the model matches the sample's average log growth.
pub fn fit_merton(series: &PriceSeries) -> Result<PathModel> {
    let lr = series.log_returns();
    if lr.len() < 20 {
        bail!("need at least 21 prices to fit a jump model");
    }
    let mut sorted = lr.clone();
    sorted.sort_by(f64::total_cmp);
    let med = quantile_sorted(&sorted, 0.5);
    let mut dev: Vec<f64> = lr.iter().map(|x| (x - med).abs()).collect();
    dev.sort_by(f64::total_cmp);
    let robust_sd = (quantile_sorted(&dev, 0.5) * 1.4826).max(1e-8);
    let thresh = 3.0 * robust_sd;
    let (jumps, normal): (Vec<f64>, Vec<f64>) = lr.iter().partition(|x| (**x - med).abs() > thresh);
    let n = lr.len() as f64;
    let (m_all, _) = mean_std(&lr);
    let (_, s_norm) = mean_std(&normal);
    let sigma = s_norm * TRADING_DAYS_PER_YEAR.sqrt();
    let lambda = jumps.len() as f64 / n * TRADING_DAYS_PER_YEAR;
    let (jump_mean, jump_std) = match jumps.len() {
        0 => (0.0, 0.0),
        1 => (jumps[0], 0.5 * jumps[0].abs()),
        _ => mean_std(&jumps),
    };
    // Mean daily log return = (μ − σ²/2 − λk)dt + λ·dt·jump_mean
    let k = (jump_mean + 0.5 * jump_std * jump_std).exp() - 1.0;
    let mu = m_all * TRADING_DAYS_PER_YEAR + 0.5 * sigma * sigma + lambda * k - lambda * jump_mean;
    Ok(PathModel::MertonJump { mu, sigma, lambda, jump_mean, jump_std })
}

/// Bootstrap model from a price history (block length in trading days, e.g. 20).
pub fn bootstrap_model(series: &PriceSeries, block: usize) -> PathModel {
    PathModel::Bootstrap { log_returns: series.log_returns(), block: block.max(1) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain::OptionKind;
    use crate::history::Bar;
    use crate::strategy::Leg;
    use chrono::Duration;

    fn d(y: i32, m: u32, dd: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, dd).unwrap()
    }

    fn series_from(prices: &[f64]) -> PriceSeries {
        let start = d(2000, 1, 3);
        let bars = prices
            .iter()
            .enumerate()
            .map(|(i, &p)| Bar { date: start + Duration::days(i as i64), open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 })
            .collect();
        PriceSeries::new("T", bars)
    }

    #[test]
    fn gbm_mean_and_vol() {
        let (mu, sigma, s0) = (0.08, 0.2, 100.0);
        let m = PathModel::Gbm { mu, sigma };
        let days = 252;
        let term = simulate_terminal(&m, s0, days, 40_000, 7);
        let mean = term.iter().sum::<f64>() / term.len() as f64;
        let expect = s0 * (mu * 1.0f64).exp();
        // SE of mean ≈ S·e^{μ}·sqrt(e^{σ²}−1)/√n ≈ 0.11
        assert!((mean - expect).abs() < 0.5, "{mean} vs {expect}");
        let lr: Vec<f64> = term.iter().map(|x| (x / s0).ln()).collect();
        let (lm, ls) = mean_std(&lr);
        assert!((ls - sigma).abs() < 0.005, "{ls}");
        assert!((lm - (mu - 0.5 * sigma * sigma)).abs() < 0.005, "{lm}");
    }

    #[test]
    fn deterministic_with_seed_and_terminal_consistent() {
        let m = PathModel::Heston { mu: 0.07, v0: 0.04, kappa: 2.0, theta: 0.04, xi: 0.5, rho: -0.7 };
        let a = simulate_paths(&m, 100.0, 50, 300, 42);
        let b = simulate_paths(&m, 100.0, 50, 300, 42);
        let c = simulate_paths(&m, 100.0, 50, 300, 43);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.iter().all(|p| p.len() == 51 && p[0] == 100.0));
        let t = simulate_terminal(&m, 100.0, 50, 300, 42);
        assert!(a.iter().zip(&t).all(|(p, x)| p[50] == *x));
    }

    #[test]
    fn merton_and_heston_are_martingale_adjusted() {
        let s0 = 100.0;
        let mer = PathModel::MertonJump { mu: 0.05, sigma: 0.15, lambda: 3.0, jump_mean: -0.05, jump_std: 0.08 };
        let term = simulate_terminal(&mer, s0, 252, 40_000, 1);
        let mean = term.iter().sum::<f64>() / term.len() as f64;
        assert!((mean / (s0 * 0.05f64.exp()) - 1.0).abs() < 0.01, "{mean}");
        let hes = PathModel::Heston { mu: 0.05, v0: 0.04, kappa: 1.5, theta: 0.05, xi: 0.6, rho: -0.7 };
        let term = simulate_terminal(&hes, s0, 252, 40_000, 2);
        let mean = term.iter().sum::<f64>() / term.len() as f64;
        assert!((mean / (s0 * 0.05f64.exp()) - 1.0).abs() < 0.01, "{mean}");
        // Negative rho → left-skewed log returns.
        let lr: Vec<f64> = term.iter().map(|x| (x / s0).ln()).collect();
        let (m, s) = mean_std(&lr);
        let skew = lr.iter().map(|x| ((x - m) / s).powi(3)).sum::<f64>() / lr.len() as f64;
        assert!(skew < -0.1, "{skew}");
    }

    #[test]
    fn bootstrap_reuses_history() {
        let lr = vec![0.01, -0.02, 0.005];
        let m = PathModel::Bootstrap { log_returns: lr.clone(), block: 2 };
        let paths = simulate_paths(&m, 100.0, 10, 50, 3);
        for p in &paths {
            for w in p.windows(2) {
                let r = (w[1] / w[0]).ln();
                assert!(lr.iter().any(|x| (x - r).abs() < 1e-12));
            }
        }
        let empty = PathModel::Bootstrap { log_returns: vec![], block: 5 };
        assert!(simulate_terminal(&empty, 100.0, 10, 3, 0).iter().all(|x| *x == 100.0));
    }

    #[test]
    fn summarize_basics() {
        let x: Vec<f64> = (1..=100).map(|i| i as f64 - 50.5).collect();
        let dist = summarize(&x);
        assert_eq!(dist.n, 100);
        assert!(dist.mean.abs() < 1e-12);
        assert!((dist.median).abs() < 1e-12);
        assert!((dist.prob_profit - 0.5).abs() < 1e-12);
        assert!((dist.p05 - quantile_sorted(&x, 0.05)).abs() < 1e-12);
        assert!(dist.var95 > 40.0 && dist.cvar95 > dist.var95);
        let total: f64 = dist.histogram.iter().map(|h| h.1).sum();
        assert!((total - 1.0).abs() < 1e-12);
        assert_eq!(summarize(&[]).n, 0);
        assert_eq!(summarize(&[3.0, 3.0]).histogram, vec![(3.0, 1.0)]);
    }

    #[test]
    fn strategy_distribution_long_call_matches_pop() {
        let created = d(2026, 1, 2);
        let e = created + Duration::days(365);
        let s = Strategy::new("lc", "QQQ", created).with_leg(Leg::option(OptionKind::Call, 105.0, e, 1.0, 6.0, 0.25));
        let p = MarketParams::new(100.0, 0.04, 0.0);
        let (mu, sigma) = (0.08, 0.25);
        let model = PathModel::Gbm { mu, sigma };
        let dist = strategy_pnl_distribution(&s, &model, 100.0, e, &p, 50_000, 11);
        // Lognormal PoP (MC uses 252 steps ≈ 1 year; 365 cal days → 252 trading days).
        let pop = s.probability_of_profit(e, &p, sigma, mu);
        assert!((dist.prob_profit - pop).abs() < 0.01, "{} vs {pop}", dist.prob_profit);
        let ev = s.expected_pnl(e, &p, sigma, mu);
        assert!((dist.mean - ev).abs() < 0.03 * ev.abs().max(100.0), "{} vs {ev}", dist.mean);
        assert!((dist.min + 600.0).abs() < 1e-6);
        // Grid interpolation agrees with direct valuation.
        let direct = strategy_pnl_samples(&s, &model, 100.0, e, &p, 1500, 5);
        let terms = simulate_terminal(&model, 100.0, 252, 1500, 5);
        for (x, st) in direct.iter().zip(&terms) {
            assert!((x - s.pnl_at(*st, e, &p)).abs() < 1e-9);
        }
    }

    #[test]
    fn american_grid_shortcut_matches_direct_valuation() {
        let created = d(2026, 1, 2);
        let s = Strategy::new("pp", "SPY", created)
            .with_leg(Leg::option(OptionKind::Put, 110.0, d(2027, 6, 18), 1.0, 14.0, 0.25))
            .with_leg(Leg::option(OptionKind::Put, 95.0, d(2026, 3, 20), -1.0, 2.0, 0.25));
        let p = MarketParams::new(100.0, 0.06, 0.0).with_american(true);
        let model = PathModel::Gbm { mu: 0.05, sigma: 0.25 };
        let h = d(2026, 2, 20);
        let n = 2500; // > grid size → interpolation path
        let fast = strategy_pnl_samples(&s, &model, 100.0, h, &p, n, 4);
        let terms = simulate_terminal(&model, 100.0, trading_days_between(created, h), n, 4);
        for i in (0..n).step_by(97) {
            let direct = s.pnl_at(terms[i], h, &p);
            assert!((fast[i] - direct).abs() < 0.5, "spot {} fast {} direct {direct}", terms[i], fast[i]);
        }
    }

    #[test]
    fn fit_models_recover_parameters() {
        let (mu, sigma) = (0.10, 0.2);
        let path = simulate_paths(&PathModel::Gbm { mu, sigma }, 100.0, 252 * 20, 1, 9).remove(0);
        if let PathModel::Gbm { mu: m, sigma: s } = fit_gbm(&series_from(&path)).unwrap() {
            assert!((s - sigma).abs() < 0.01, "{s}");
            assert!((m - mu).abs() < 0.12, "{m}");
        } else {
            panic!()
        }
        let true_m = PathModel::MertonJump { mu: 0.08, sigma: 0.15, lambda: 4.0, jump_mean: -0.06, jump_std: 0.03 };
        let path = simulate_paths(&true_m, 100.0, 252 * 30, 1, 17).remove(0);
        if let PathModel::MertonJump { sigma, lambda, jump_mean, .. } = fit_merton(&series_from(&path)).unwrap() {
            assert!((sigma - 0.15).abs() < 0.02, "{sigma}");
            assert!((lambda - 4.0).abs() < 1.5, "{lambda}");
            assert!((jump_mean + 0.06).abs() < 0.02, "{jump_mean}");
        } else {
            panic!()
        }
        assert!(fit_gbm(&series_from(&[1.0, 2.0])).is_err());
        let q = path_quantiles(&simulate_paths(&PathModel::Gbm { mu, sigma }, 100.0, 20, 500, 1), 5);
        assert_eq!(q.last().unwrap().step, 20);
        assert!(q.iter().all(|x| x.p05 <= x.p50 && x.p50 <= x.p95));
    }

    #[test]
    fn perf_20k_by_504() {
        // Smoke test for speed; release target is well under a second.
        let m = PathModel::Gbm { mu: 0.08, sigma: 0.2 };
        let t0 = std::time::Instant::now();
        let term = simulate_terminal(&m, 100.0, 504, 20_000, 1);
        assert_eq!(term.len(), 20_000);
        let t_term = t0.elapsed().as_secs_f64();
        let t1 = std::time::Instant::now();
        let paths = simulate_paths(&PathModel::Heston { mu: 0.07, v0: 0.04, kappa: 2.0, theta: 0.04, xi: 0.5, rho: -0.7 }, 100.0, 504, 20_000, 1);
        assert_eq!(paths.len(), 20_000);
        let t_paths = t1.elapsed().as_secs_f64();
        // PMCC with American marking at the short-leg expiry.
        let created = d(2026, 11, 4);
        let s = Strategy::new("pmcc", "QQQ", created)
            .with_leg(Leg::option(OptionKind::Call, 80.0, d(2028, 6, 16), 1.0, 25.0, 0.22))
            .with_leg(Leg::option(OptionKind::Call, 105.0, d(2026, 12, 18), -1.0, 1.2, 0.2));
        let p = MarketParams::new(100.0, 0.04, 0.006).with_american(true);
        let t2 = std::time::Instant::now();
        let dist = strategy_pnl_distribution(&s, &m, 100.0, d(2026, 12, 18), &p, 20_000, 3);
        let t_strat = t2.elapsed().as_secs_f64();
        assert_eq!(dist.n, 20_000);
        eprintln!("GBM terminal 20k×504: {t_term:.3}s, Heston paths 20k×504: {t_paths:.3}s, PMCC american dist: {t_strat:.3}s");
        assert!(t_term < 10.0);
    }
}
