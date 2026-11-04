//! Parameter search and robustness analysis for the LEAPS roll strategy.
//!
//! * [`optimize`] — parallel grid search (rayon), rows sorted best-first.
//! * [`walk_forward`] — rolling train/test re-optimisation to measure how much of
//!   the in-sample edge survives out of sample.
//! * [`bootstrap_robustness`] — stationary block bootstrap of **joint** (daily log
//!   return, daily vol-index log change) pairs ⇒ synthetic histories that keep the
//!   leverage effect (vol spikes when prices fall), then re-run the backtest.
//! * [`optimize_robust`] — grid search scored on a quantile of the objective over
//!   common bootstrap paths (+ the real history), optionally smoothed over grid
//!   neighbours, so the winner is not single-path noise.
//! * [`sensitivity`] / [`heatmap`] — 1-D sweeps and delta × DTE grids for charts.
//!
//! Rows whose realised average exposure is below [`MIN_EXPOSURE_RATIO`] of the target
//! (e.g. because `max_premium_pct` binds) are flagged (`exposure_ok = false`) and
//! ranked after all rows that achieve their target.

use anyhow::{bail, Result};
use chrono::{Duration, NaiveDate};
use rand::{rngs::StdRng, Rng, SeedableRng};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::backtest::{
    buy_hold_curve, exposure_matched_curve, exposure_shortfall, leveraged_curve, result_label, simulate, Aligned, BacktestData, RollConfig,
    StrikeSelection,
};
use crate::stats::{compute_stats, quantile, PerformanceStats};

/// Search grid. Empty vectors mean "keep the base config's value".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamGrid {
    pub deltas: Vec<f64>,
    pub entry_dtes: Vec<u32>,
    pub roll_dtes: Vec<u32>,
    pub exposures: Vec<f64>,
    pub roll_up_deltas: Vec<Option<f64>>,
    /// Optional extra dimension (empty = base `rebalance_band`).
    pub rebalance_bands: Vec<Option<f64>>,
    /// Combos with `entry_dte − roll_dte < min_hold_days` are skipped.
    pub min_hold_days: u32,
}

impl Default for ParamGrid {
    /// 4 deltas × 13 valid DTE pairs × 5 exposures × 2 roll-up rules = 520 combos.
    fn default() -> Self {
        Self {
            deltas: vec![0.60, 0.70, 0.80, 0.90],
            entry_dtes: vec![270, 365, 540, 730],
            roll_dtes: vec![60, 120, 180, 270],
            exposures: vec![1.0, 1.25, 1.5, 2.0, 2.5],
            roll_up_deltas: vec![None, Some(0.95)],
            rebalance_bands: vec![],
            min_hold_days: 90,
        }
    }
}

impl ParamGrid {
    /// A small grid (~36 combos) for quick GUI previews and walk-forward.
    pub fn small() -> Self {
        Self {
            deltas: vec![0.70, 0.80, 0.90],
            entry_dtes: vec![365, 540, 730],
            roll_dtes: vec![120, 240],
            exposures: vec![1.0, 1.5, 2.0],
            roll_up_deltas: vec![None],
            rebalance_bands: vec![],
            min_hold_days: 90,
        }
    }

    /// Expand into concrete configs (base fields kept for everything not in the grid).
    pub fn configs(&self, base: &RollConfig) -> Vec<RollConfig> {
        self.configs_indexed(base).into_iter().map(|(_, c)| c).collect()
    }

    /// Like [`configs`](Self::configs) with each combo's index in every grid dimension
    /// (strike, entry, roll, exposure, roll-up, band).
    fn configs_indexed(&self, base: &RollConfig) -> Vec<([usize; 6], RollConfig)> {
        let strikes: Vec<StrikeSelection> = if self.deltas.is_empty() {
            vec![base.strike]
        } else {
            self.deltas.iter().map(|&d| StrikeSelection::Delta(d)).collect()
        };
        let or = |v: &Vec<u32>, b: u32| if v.is_empty() { vec![b] } else { v.clone() };
        let entries = or(&self.entry_dtes, base.entry_dte);
        let rolls = or(&self.roll_dtes, base.roll_dte);
        let exps = if self.exposures.is_empty() { vec![base.target_exposure] } else { self.exposures.clone() };
        let ups = if self.roll_up_deltas.is_empty() { vec![base.roll_up_delta] } else { self.roll_up_deltas.clone() };
        let bands = if self.rebalance_bands.is_empty() { vec![base.rebalance_band] } else { self.rebalance_bands.clone() };
        let mut out = Vec::new();
        for (si, &strike) in strikes.iter().enumerate() {
            for (ei, &e) in entries.iter().enumerate() {
                for (ri, &r) in rolls.iter().enumerate() {
                    if e <= r || e - r < self.min_hold_days {
                        continue;
                    }
                    for (xi, &x) in exps.iter().enumerate() {
                        for (ui, &u) in ups.iter().enumerate() {
                            // a roll-up threshold below the entry delta would roll every day
                            if let (Some(u), StrikeSelection::Delta(d)) = (u, strike) {
                                if u <= d + 0.02 {
                                    continue;
                                }
                            }
                            for (bi, &b) in bands.iter().enumerate() {
                                out.push(([si, ei, ri, xi, ui, bi], RollConfig {
                                    strike,
                                    entry_dte: e,
                                    roll_dte: r,
                                    target_exposure: x,
                                    roll_up_delta: u,
                                    rebalance_band: b,
                                    ..base.clone()
                                }));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    pub fn len(&self, base: &RollConfig) -> usize {
        self.configs(base).len()
    }
}

/// What to maximise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Objective {
    Cagr,
    Sharpe,
    Calmar,
    Sortino,
    /// CAGR − λ·|MaxDD|.
    Utility { lambda: f64 },
    /// Max CAGR subject to |MaxDD| ≤ max_dd (violations rank below all feasible rows).
    MaxCagrWithDdCap { max_dd: f64 },
}

impl Default for Objective {
    fn default() -> Self {
        Objective::Utility { lambda: 0.25 }
    }
}

/// Score assigned to wiped-out runs (plus their CAGR, i.e. ≈ −101).
pub const WIPEOUT_SCORE: f64 = -100.0;

impl Objective {
    pub fn score(&self, s: &PerformanceStats, wiped: bool) -> f64 {
        if wiped {
            return WIPEOUT_SCORE + s.cagr;
        }
        let v = match *self {
            Objective::Cagr => s.cagr,
            Objective::Sharpe => s.sharpe,
            Objective::Calmar => s.calmar,
            Objective::Sortino => s.sortino,
            Objective::Utility { lambda } => s.cagr - lambda * s.max_drawdown.abs(),
            Objective::MaxCagrWithDdCap { max_dd } => {
                let dd = s.max_drawdown.abs();
                if dd <= max_dd {
                    s.cagr
                } else {
                    s.cagr - 1.0 - (dd - max_dd)
                }
            }
        };
        if v.is_finite() {
            v
        } else {
            WIPEOUT_SCORE
        }
    }

    /// Traditional-Chinese label for UIs.
    pub fn label(&self) -> String {
        match *self {
            Objective::Cagr => "年化報酬 CAGR".into(),
            Objective::Sharpe => "夏普比率".into(),
            Objective::Calmar => "Calmar（CAGR/最大回撤）".into(),
            Objective::Sortino => "Sortino".into(),
            Objective::Utility { lambda } => format!("效用 CAGR − {lambda:.2}×|MDD|"),
            Objective::MaxCagrWithDdCap { max_dd } => format!("最大回撤 ≤ {:.0}% 下最大化 CAGR", max_dd * 100.0),
        }
    }
}

/// One evaluated configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptRow {
    pub config: RollConfig,
    /// Statistics on the actual history.
    pub stats: PerformanceStats,
    /// Ranking score (for [`optimize_robust`]: the robust score).
    pub score: f64,
    pub n_rolls: usize,
    pub total_costs: f64,
    pub avg_exposure: f64,
    pub avg_extrinsic_pct_annual: f64,
    pub wiped_out: bool,
    /// Lowest equity / initial capital on the actual history.
    #[serde(default)]
    pub min_equity_ratio: f64,
    /// `min_equity_ratio < config.near_wipe_level`.
    #[serde(default)]
    pub near_wipe: bool,
    /// 1 − avg_exposure / target_exposure (positive ⇒ under-exposed).
    #[serde(default)]
    pub exposure_shortfall: f64,
    /// avg_exposure ≥ [`MIN_EXPOSURE_RATIO`] × target.
    #[serde(default = "default_true")]
    pub exposure_ok: bool,
    /// Objective score on the actual history alone.
    #[serde(default)]
    pub history_score: f64,
    /// Interquartile range of the per-path scores ([`optimize_robust`]; 0 otherwise).
    #[serde(default)]
    pub score_iqr: f64,
}

fn default_true() -> bool {
    true
}

/// Minimum realised/target exposure ratio for a row to count as achieving its target.
pub const MIN_EXPOSURE_RATIO: f64 = 0.85;

impl OptRow {
    /// Config label with realised exposure (when it deviates > 5%) and survival markers.
    pub fn label(&self) -> String {
        result_label(&self.config, self.avg_exposure, self.min_equity_ratio, self.near_wipe, self.wiped_out)
    }
}

fn eval_one(al: &Aligned, cfg: &RollConfig, obj: Objective) -> Option<OptRow> {
    let sim = simulate(al, cfg, false).ok()?;
    let stats = compute_stats(&sim.curve, sim.avg_rate);
    let score = obj.score(&stats, sim.wiped_out);
    let shortfall = exposure_shortfall(sim.avg_exposure, cfg.target_exposure);
    Some(OptRow {
        config: cfg.clone(),
        stats,
        score,
        n_rolls: sim.n_rolls,
        total_costs: sim.total_costs,
        avg_exposure: sim.avg_exposure,
        avg_extrinsic_pct_annual: sim.avg_extrinsic_pct_annual,
        wiped_out: sim.wiped_out,
        min_equity_ratio: sim.min_equity_ratio,
        near_wipe: sim.near_wipe,
        exposure_shortfall: shortfall,
        exposure_ok: cfg.target_exposure <= 0.0 || 1.0 - shortfall >= MIN_EXPOSURE_RATIO,
        history_score: score,
        score_iqr: 0.0,
    })
}

/// Rows achieving their exposure target first, then by score (best first).
fn sort_rows(rows: &mut [OptRow]) {
    rows.sort_by(|a, b| {
        b.exposure_ok
            .cmp(&a.exposure_ok)
            .then(b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
    });
}

pub(crate) fn optimize_aligned(
    al: &Aligned,
    base: &RollConfig,
    grid: &ParamGrid,
    objective: Objective,
    progress: Option<&(dyn Fn(usize, usize) + Sync)>,
) -> Vec<OptRow> {
    let cfgs = grid.configs(base);
    let total = cfgs.len();
    let done = AtomicUsize::new(0);
    let mut rows: Vec<OptRow> = cfgs
        .par_iter()
        .filter_map(|c| {
            let r = eval_one(al, c, objective);
            let k = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(p) = progress {
                p(k, total);
            }
            r
        })
        .collect();
    sort_rows(&mut rows);
    rows
}

/// Grid search; rows sorted best-first by `objective`. Invalid combos are skipped.
pub fn optimize(
    data: &BacktestData,
    base: &RollConfig,
    grid: &ParamGrid,
    objective: Objective,
    progress: Option<&(dyn Fn(usize, usize) + Sync)>,
) -> Vec<OptRow> {
    match Aligned::new(data) {
        Ok(al) => optimize_aligned(&al, base, grid, objective, progress),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Robust optimisation
// ---------------------------------------------------------------------------

/// Options for [`optimize_robust`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RobustOptions {
    /// Bootstrap paths (common random numbers: every combo sees the same paths).
    pub n_paths: usize,
    /// Mean block length of the stationary bootstrap (trading days).
    pub block_days: f64,
    pub seed: u64,
    /// Also score the actual history as one more path.
    pub include_history: bool,
    /// Quantile of the per-path scores used as the score (0.5 = median, 0.25 = cautious).
    pub quantile: f64,
    /// Blend each combo's score with its grid neighbours (one step in one dimension).
    pub smooth_neighbours: bool,
    /// Weight of the combo itself when smoothing (the rest is split over neighbours).
    pub self_weight: f64,
}

impl Default for RobustOptions {
    /// 12 paths + history, median, neighbour smoothing 50/50. The default 600-combo
    /// grid on 27 years of daily data takes a few seconds on a 4-core machine.
    fn default() -> Self {
        Self { n_paths: 12, block_days: 60.0, seed: 7, include_history: true, quantile: 0.5, smooth_neighbours: true, self_weight: 0.5 }
    }
}

/// Grid search scored on the `quantile` of `objective` across bootstrap paths (joint
/// return/vol-index blocks, same as [`bootstrap_robustness`]) plus the actual history,
/// optionally smoothed over grid neighbours. `stats` / `history_score` in each row still
/// refer to the actual history; `score` is the robust score; `score_iqr` its dispersion.
pub fn optimize_robust(
    data: &BacktestData,
    base: &RollConfig,
    grid: &ParamGrid,
    objective: Objective,
    opts: &RobustOptions,
    progress: Option<&(dyn Fn(usize, usize) + Sync)>,
) -> Result<Vec<OptRow>> {
    let al = Aligned::new(data)?;
    let Some((i0, i1)) = al.range(base.start, base.end) else {
        bail!("no data in range");
    };
    if i1 - i0 < 30 {
        bail!("need at least 30 days of history");
    }
    let paths: Vec<Aligned> = (0..opts.n_paths)
        .map(|k| {
            let mut rng = StdRng::seed_from_u64(opts.seed ^ (k as u64).wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            synthetic_history(&al, i0, i1, opts.block_days, &mut rng)
        })
        .collect();
    let indexed = grid.configs_indexed(base);
    let total = indexed.len();
    let done = AtomicUsize::new(0);
    let q = opts.quantile.clamp(0.0, 1.0);
    let mut rows: Vec<([usize; 6], OptRow)> = indexed
        .par_iter()
        .filter_map(|(idx, c)| {
            let mut row = eval_one(&al, c, objective)?;
            let free = RollConfig { start: None, end: None, ..c.clone() };
            let mut scores: Vec<f64> = paths
                .iter()
                .filter_map(|p| {
                    let sim = simulate(p, &free, false).ok()?;
                    Some(objective.score(&compute_stats(&sim.curve, sim.avg_rate), sim.wiped_out))
                })
                .collect();
            if opts.include_history || scores.is_empty() {
                scores.push(row.history_score);
            }
            row.score = quantile(&scores, q);
            row.score_iqr = quantile(&scores, 0.75) - quantile(&scores, 0.25);
            let k = done.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(p) = progress {
                p(k, total);
            }
            Some((*idx, row))
        })
        .collect();
    if opts.smooth_neighbours && rows.len() > 1 {
        let map: std::collections::HashMap<[usize; 6], f64> = rows.iter().map(|(i, r)| (*i, r.score)).collect();
        let w = opts.self_weight.clamp(0.0, 1.0);
        for (idx, row) in rows.iter_mut() {
            let mut nb = Vec::new();
            for dim in 0..6 {
                for step in [-1i64, 1] {
                    let v = idx[dim] as i64 + step;
                    if v < 0 {
                        continue;
                    }
                    let mut j = *idx;
                    j[dim] = v as usize;
                    if let Some(sc) = map.get(&j) {
                        nb.push(*sc);
                    }
                }
            }
            if !nb.is_empty() {
                let mean = nb.iter().sum::<f64>() / nb.len() as f64;
                row.score = w * row.score + (1.0 - w) * mean;
            }
        }
    }
    let mut out: Vec<OptRow> = rows.into_iter().map(|(_, r)| r).collect();
    sort_rows(&mut out);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Walk-forward
// ---------------------------------------------------------------------------

/// One train/test window of a walk-forward analysis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardWindow {
    pub train_start: NaiveDate,
    pub train_end: NaiveDate,
    pub test_start: NaiveDate,
    pub test_end: NaiveDate,
    /// Best config on the training window (its start/end are set to the test window).
    pub best_config: RollConfig,
    pub in_sample_score: f64,
    pub in_sample_stats: PerformanceStats,
    pub oos_stats: PerformanceStats,
    pub oos_score: f64,
    /// The un-optimised `base` config on the same test window.
    pub base_oos_stats: PerformanceStats,
    /// Buy & hold on the test window.
    pub bh_oos_stats: PerformanceStats,
}

/// Chained out-of-sample results of a walk-forward run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WalkForwardSummary {
    pub years: f64,
    pub oos_cagr: f64,
    pub base_cagr: f64,
    pub bh_cagr: f64,
    pub worst_oos_dd: f64,
    pub worst_base_dd: f64,
    pub worst_bh_dd: f64,
    /// Fraction of windows where the optimised config beat the base config OOS.
    pub pct_windows_opt_beats_base: f64,
}

/// Rolling re-optimisation: optimise on `train_years`, test on the next `test_years`,
/// step forward by `test_years`. The final test window may be shorter (≥ half).
pub fn walk_forward(
    data: &BacktestData,
    base: &RollConfig,
    grid: &ParamGrid,
    objective: Objective,
    train_years: f64,
    test_years: f64,
) -> Result<Vec<WalkForwardWindow>> {
    if !(train_years > 0.0 && test_years > 0.0) {
        bail!("train_years and test_years must be > 0");
    }
    let al = Aligned::new(data)?;
    let Some((i0, i1)) = al.range(base.start, base.end) else {
        bail!("no data in range");
    };
    let first = al.dates[i0];
    let last = al.dates[i1];
    let tr = Duration::days((train_years * 365.25) as i64);
    let te = Duration::days((test_years * 365.25) as i64);
    let mut out = Vec::new();
    let mut ts = first;
    loop {
        let train_end = ts + tr;
        let test_start = train_end + Duration::days(1);
        if test_start >= last {
            break;
        }
        let mut test_end = test_start + te;
        if test_end > last {
            if (last - test_start).num_days() < te.num_days() / 2 {
                break;
            }
            test_end = last;
        }
        let train_base = RollConfig { start: Some(ts), end: Some(train_end), ..base.clone() };
        let rows = optimize_aligned(&al, &train_base, grid, objective, None);
        if let Some(best) = rows.first() {
            let test_cfg = RollConfig { start: Some(test_start), end: Some(test_end), ..best.config.clone() };
            let base_cfg = RollConfig { start: Some(test_start), end: Some(test_end), ..base.clone() };
            if let (Some(oos), Some(bo)) = (eval_one(&al, &test_cfg, objective), eval_one(&al, &base_cfg, objective)) {
                let (j0, j1) = al.range(Some(test_start), Some(test_end)).unwrap();
                let bh = compute_stats(&buy_hold_curve(&al, j0, j1, 1.0), 0.0);
                out.push(WalkForwardWindow {
                    train_start: ts,
                    train_end,
                    test_start,
                    test_end,
                    best_config: test_cfg,
                    in_sample_score: best.score,
                    in_sample_stats: best.stats.clone(),
                    oos_score: oos.score,
                    oos_stats: oos.stats,
                    base_oos_stats: bo.stats,
                    bh_oos_stats: bh,
                });
            }
        }
        ts += te;
    }
    Ok(out)
}

/// Chain the out-of-sample windows into one summary.
pub fn walk_forward_summary(windows: &[WalkForwardWindow]) -> WalkForwardSummary {
    let mut s = WalkForwardSummary::default();
    if windows.is_empty() {
        return s;
    }
    let (mut g_o, mut g_b, mut g_h) = (1.0, 1.0, 1.0);
    let mut wins = 0;
    for w in windows {
        s.years += w.oos_stats.years;
        g_o *= 1.0 + w.oos_stats.total_return;
        g_b *= 1.0 + w.base_oos_stats.total_return;
        g_h *= 1.0 + w.bh_oos_stats.total_return;
        s.worst_oos_dd = s.worst_oos_dd.min(w.oos_stats.max_drawdown);
        s.worst_base_dd = s.worst_base_dd.min(w.base_oos_stats.max_drawdown);
        s.worst_bh_dd = s.worst_bh_dd.min(w.bh_oos_stats.max_drawdown);
        if w.oos_stats.total_return > w.base_oos_stats.total_return {
            wins += 1;
        }
    }
    let ann = |g: f64| if g > 0.0 && s.years > 0.0 { g.powf(1.0 / s.years) - 1.0 } else { -1.0 };
    s.oos_cagr = ann(g_o);
    s.base_cagr = ann(g_b);
    s.bh_cagr = ann(g_h);
    s.pct_windows_opt_beats_base = wins as f64 / windows.len() as f64;
    s
}

// ---------------------------------------------------------------------------
// Bootstrap robustness
// ---------------------------------------------------------------------------

/// Distribution summary.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Percentiles {
    pub p5: f64,
    pub p25: f64,
    pub p50: f64,
    pub p75: f64,
    pub p95: f64,
    pub mean: f64,
}

impl Percentiles {
    pub fn from_samples(x: &[f64]) -> Self {
        let fin: Vec<f64> = x.iter().copied().filter(|v| v.is_finite()).collect();
        let mean = if fin.is_empty() { f64::NAN } else { fin.iter().sum::<f64>() / fin.len() as f64 };
        Self {
            p5: quantile(&fin, 0.05),
            p25: quantile(&fin, 0.25),
            p50: quantile(&fin, 0.50),
            p75: quantile(&fin, 0.75),
            p95: quantile(&fin, 0.95),
            mean,
        }
    }
}

/// Result of [`bootstrap_robustness`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RobustnessReport {
    pub n_samples: usize,
    pub block_days: f64,
    pub years: f64,
    pub cagr: Percentiles,
    pub max_dd: Percentiles,
    pub sharpe: Percentiles,
    /// Buy & hold on the same synthetic paths.
    pub bh_cagr: Percentiles,
    pub bh_max_dd: Percentiles,
    /// Daily-rebalanced L× (same target exposure) on the same paths.
    pub lev_cagr: Percentiles,
    pub lev_max_dd: Percentiles,
    pub prob_wipeout: f64,
    /// P(strategy CAGR > buy & hold CAGR).
    pub prob_beat_benchmark: f64,
    /// P(strategy CAGR > daily-rebalanced L× CAGR).
    pub prob_beat_lev_etf: f64,
    /// P(final equity < initial capital).
    pub prob_loss: f64,
    /// P(max drawdown worse than −50%).
    pub prob_dd_over_50: f64,
    pub cagr_samples: Vec<f64>,
    pub max_dd_samples: Vec<f64>,
    /// P(min equity < `config.near_wipe_level`, default 10% of initial capital).
    #[serde(default)]
    pub prob_near_wipe: f64,
    /// Lowest equity / initial capital per path.
    #[serde(default)]
    pub min_equity: Percentiles,
    /// Exposure-matched linear benchmark on the same paths.
    #[serde(default)]
    pub matched_cagr: Percentiles,
    /// P(strategy CAGR > exposure-matched linear CAGR): the option-specific edge.
    #[serde(default)]
    pub prob_beat_matched: f64,
}

/// Anchor strength pulling the synthetic vol path back to the source block's level
/// (per day). 0.05 ⇒ ~14-day half-life after a block jump, while inside a block the
/// historical vol changes are reproduced exactly.
pub const BOOT_VOL_ANCHOR: f64 = 0.05;

/// One stationary-bootstrap synthetic history on the dates of `[i0, i1]`.
pub(crate) fn synthetic_history(al: &Aligned, i0: usize, i1: usize, block_days: f64, rng: &mut StdRng) -> Aligned {
    let n = i1 - i0; // number of daily changes
    let p_new = 1.0 / block_days.max(1.0);
    let mut adj = Vec::with_capacity(n + 1);
    let mut vol = Vec::with_capacity(n + 1);
    let mut a = al.adj[i0];
    let mut lv = al.vol[i0].ln();
    adj.push(a);
    vol.push(al.vol[i0]);
    let mut idx = rng.gen_range(0..n);
    for t in 0..n {
        if t > 0 {
            idx = if rng.gen::<f64>() < p_new { rng.gen_range(0..n) } else { (idx + 1) % n };
        }
        let j = i0 + idx;
        a *= al.adj[j + 1] / al.adj[j];
        let src_lv = al.vol[j + 1].ln();
        let next = lv + (al.vol[j + 1] / al.vol[j]).ln();
        lv = next + BOOT_VOL_ANCHOR * (src_lv - next);
        lv = lv.clamp(4f64.ln(), 150f64.ln());
        adj.push(a);
        vol.push(lv.exp());
    }
    Aligned::from_parts(
        al.symbol.clone(),
        al.dates[i0..=i1].to_vec(),
        adj,
        vol,
        al.rate[i0..=i1].to_vec(),
        al.q,
    )
}

struct BootSample {
    min_eq: f64,
    near_wipe: bool,
    matched_cagr: f64,
    cagr: f64,
    mdd: f64,
    sharpe: f64,
    wiped: bool,
    loss: bool,
    bh_cagr: f64,
    bh_mdd: f64,
    lev_cagr: f64,
    lev_mdd: f64,
    years: f64,
}

/// Block-bootstrap robustness of one configuration. Deterministic for a given seed.
pub fn bootstrap_robustness(data: &BacktestData, config: &RollConfig, n_samples: usize, block_days: f64, seed: u64) -> Result<RobustnessReport> {
    let al = Aligned::new(data)?;
    let Some((i0, i1)) = al.range(config.start, config.end) else {
        bail!("no data in range");
    };
    if i1 - i0 < 30 {
        bail!("need at least 30 days of history to bootstrap");
    }
    crate::backtest::validate(config)?;
    let cfg = RollConfig { start: None, end: None, ..config.clone() };
    let res: Vec<BootSample> = (0..n_samples)
        .into_par_iter()
        .filter_map(|k| {
            let mut rng = StdRng::seed_from_u64(seed ^ (k as u64).wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let syn = synthetic_history(&al, i0, i1, block_days, &mut rng);
            let sim = simulate(&syn, &cfg, false).ok()?;
            let st = compute_stats(&sim.curve, sim.avg_rate);
            let m = syn.dates.len() - 1;
            let bh = compute_stats(&buy_hold_curve(&syn, 0, m, 1.0), sim.avg_rate);
            let lev = compute_stats(
                &leveraged_curve(&syn, 0, m, 1.0, cfg.target_exposure, cfg.lev_borrow_spread, cfg.lev_expense_ratio),
                sim.avg_rate,
            );
            let matched = compute_stats(&exposure_matched_curve(&syn, 0, m, 1.0, &sim.exposures, cfg.option_rate_spread), sim.avg_rate);
            Some(BootSample {
                min_eq: sim.min_equity_ratio,
                near_wipe: sim.near_wipe,
                matched_cagr: matched.cagr,
                cagr: st.cagr,
                mdd: st.max_drawdown,
                sharpe: st.sharpe,
                wiped: sim.wiped_out,
                loss: st.final_value < cfg.initial_capital,
                bh_cagr: bh.cagr,
                bh_mdd: bh.max_drawdown,
                lev_cagr: lev.cagr,
                lev_mdd: lev.max_drawdown,
                years: st.years,
            })
        })
        .collect();
    if res.is_empty() {
        bail!("bootstrap produced no valid samples");
    }
    let n = res.len() as f64;
    let col = |f: &dyn Fn(&BootSample) -> f64| res.iter().map(f).collect::<Vec<f64>>();
    let cagr = col(&|r| r.cagr);
    let mdd = col(&|r| r.mdd);
    Ok(RobustnessReport {
        n_samples: res.len(),
        block_days,
        years: res[0].years,
        cagr: Percentiles::from_samples(&cagr),
        max_dd: Percentiles::from_samples(&mdd),
        sharpe: Percentiles::from_samples(&col(&|r| r.sharpe)),
        bh_cagr: Percentiles::from_samples(&col(&|r| r.bh_cagr)),
        bh_max_dd: Percentiles::from_samples(&col(&|r| r.bh_mdd)),
        lev_cagr: Percentiles::from_samples(&col(&|r| r.lev_cagr)),
        lev_max_dd: Percentiles::from_samples(&col(&|r| r.lev_mdd)),
        prob_wipeout: res.iter().filter(|r| r.wiped).count() as f64 / n,
        prob_beat_benchmark: res.iter().filter(|r| r.cagr > r.bh_cagr).count() as f64 / n,
        prob_beat_lev_etf: res.iter().filter(|r| r.cagr > r.lev_cagr).count() as f64 / n,
        prob_loss: res.iter().filter(|r| r.loss).count() as f64 / n,
        prob_dd_over_50: res.iter().filter(|r| r.mdd < -0.5).count() as f64 / n,
        prob_near_wipe: res.iter().filter(|r| r.near_wipe).count() as f64 / n,
        min_equity: Percentiles::from_samples(&col(&|r| r.min_eq)),
        matched_cagr: Percentiles::from_samples(&col(&|r| r.matched_cagr)),
        prob_beat_matched: res.iter().filter(|r| r.cagr > r.matched_cagr).count() as f64 / n,
        cagr_samples: cagr,
        max_dd_samples: mdd,
    })
}

// ---------------------------------------------------------------------------
// Sensitivity & heatmaps
// ---------------------------------------------------------------------------

/// Parameter for 1-D sweeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SweepParam {
    Delta,
    Moneyness,
    EntryDte,
    RollDte,
    Exposure,
    /// Value ≤ 0 ⇒ None.
    RollUpDelta,
    /// Value ≤ 0 ⇒ None.
    RebalanceBand,
    /// Value ≤ 0 ⇒ None.
    ProfitTake,
    MaxPremiumPct,
    HalfSpreadPct,
    IvScale,
}

impl SweepParam {
    pub fn label(&self) -> &'static str {
        match self {
            SweepParam::Delta => "Delta",
            SweepParam::Moneyness => "履約價/現價",
            SweepParam::EntryDte => "建倉天數 DTE",
            SweepParam::RollDte => "展期門檻 DTE",
            SweepParam::Exposure => "目標曝險倍數",
            SweepParam::RollUpDelta => "上移 Delta 門檻",
            SweepParam::RebalanceBand => "再平衡區間",
            SweepParam::ProfitTake => "獲利了結 %",
            SweepParam::MaxPremiumPct => "權利金上限 %",
            SweepParam::HalfSpreadPct => "半價差 %",
            SweepParam::IvScale => "IV 倍數",
        }
    }
}

/// Set one parameter on a config.
pub fn apply_param(cfg: &mut RollConfig, param: SweepParam, v: f64) {
    let opt = |v: f64| if v > 0.0 { Some(v) } else { None };
    match param {
        SweepParam::Delta => cfg.strike = StrikeSelection::Delta(v),
        SweepParam::Moneyness => cfg.strike = StrikeSelection::Moneyness(v),
        SweepParam::EntryDte => cfg.entry_dte = v.round().max(0.0) as u32,
        SweepParam::RollDte => cfg.roll_dte = v.round().max(0.0) as u32,
        SweepParam::Exposure => cfg.target_exposure = v,
        SweepParam::RollUpDelta => cfg.roll_up_delta = opt(v),
        SweepParam::RebalanceBand => cfg.rebalance_band = opt(v),
        SweepParam::ProfitTake => cfg.profit_take_pct = opt(v),
        SweepParam::MaxPremiumPct => cfg.max_premium_pct = v,
        SweepParam::HalfSpreadPct => cfg.costs.half_spread_pct = v,
        SweepParam::IvScale => cfg.vol_model.iv_scale = v,
    }
}

/// 1-D sweep; invalid values (e.g. roll_dte ≥ entry_dte) are skipped.
pub fn sensitivity(data: &BacktestData, base: &RollConfig, param: SweepParam, values: &[f64]) -> Vec<(f64, PerformanceStats)> {
    let Ok(al) = Aligned::new(data) else {
        return Vec::new();
    };
    values
        .par_iter()
        .filter_map(|&v| {
            let mut c = base.clone();
            apply_param(&mut c, param, v);
            eval_one(&al, &c, Objective::Cagr).map(|r| (v, r.stats))
        })
        .collect()
}

/// Objective score over a delta × entry-DTE grid: `out[i][j]` ↔ (`deltas[i]`, `dtes[j]`).
/// The roll threshold is `min(base.roll_dte, dte/2)`. Invalid cells are NaN.
pub fn heatmap(data: &BacktestData, base: &RollConfig, deltas: &[f64], dtes: &[u32], objective: Objective) -> Vec<Vec<f64>> {
    let Ok(al) = Aligned::new(data) else {
        return vec![vec![f64::NAN; dtes.len()]; deltas.len()];
    };
    deltas
        .par_iter()
        .map(|&d| {
            dtes.iter()
                .map(|&t| {
                    let c = RollConfig { strike: StrikeSelection::Delta(d), entry_dte: t, roll_dte: base.roll_dte.min(t / 2), ..base.clone() };
                    eval_one(&al, &c, objective).map_or(f64::NAN, |r| r.score)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::test_util::gbm_data;

    #[test]
    fn default_grid_size() {
        let n = ParamGrid::default().len(&RollConfig::default());
        assert!((200..=800).contains(&n), "{n}");
    }

    #[test]
    fn optimizer_sorted_and_objectives() {
        let data = gbm_data(8.0, 0.09, 0.2, 20.0, 0.02, 0.01, 21);
        let rows = optimize(&data, &RollConfig::default(), &ParamGrid::small(), Objective::Sharpe, None);
        assert!(!rows.is_empty());
        assert!(rows.windows(2).all(|w| w[0].score >= w[1].score));
        assert!((rows[0].score - rows[0].stats.sharpe).abs() < 1e-12);
        let calls = AtomicUsize::new(0);
        let cb = |_d: usize, _t: usize| {
            calls.fetch_add(1, Ordering::Relaxed);
        };
        let rows2 = optimize(&data, &RollConfig::default(), &ParamGrid::small(), Objective::Cagr, Some(&cb));
        assert_eq!(calls.load(Ordering::Relaxed), ParamGrid::small().len(&RollConfig::default()));
        assert!(rows2.windows(2).all(|w| w[0].stats.cagr >= w[1].stats.cagr));
        let s = PerformanceStats { cagr: 0.2, max_drawdown: -0.5, ..Default::default() };
        assert!(Objective::MaxCagrWithDdCap { max_dd: 0.4 }.score(&s, false) < -0.5);
        assert!((Objective::Utility { lambda: 0.2 }.score(&s, false) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn robust_optimizer_and_flags() {
        let data = gbm_data(8.0, 0.09, 0.2, 20.0, 0.02, 0.01, 21);
        let grid = ParamGrid { exposures: vec![1.0, 1.5], ..ParamGrid::small() };
        let opts = RobustOptions { n_paths: 4, ..RobustOptions::default() };
        let a = optimize_robust(&data, &RollConfig::default(), &grid, Objective::Calmar, &opts, None).unwrap();
        let b = optimize_robust(&data, &RollConfig::default(), &grid, Objective::Calmar, &opts, None).unwrap();
        assert_eq!(a.len(), grid.len(&RollConfig::default()));
        assert_eq!(a.iter().map(|r| r.score).collect::<Vec<_>>(), b.iter().map(|r| r.score).collect::<Vec<_>>());
        assert!(a.windows(2).all(|w| (w[0].exposure_ok, w[0].score) >= (w[1].exposure_ok, w[1].score)));
        assert!(a.iter().all(|r| r.score_iqr >= 0.0 && r.min_equity_ratio > 0.0));
        // a combo that cannot reach its target exposure is flagged
        let cfg = RollConfig { strike: StrikeSelection::Delta(0.95), target_exposure: 4.0, max_premium_pct: 0.5, ..RollConfig::default() };
        let rows = optimize(&data, &cfg, &ParamGrid { deltas: vec![], entry_dtes: vec![], roll_dtes: vec![], exposures: vec![], roll_up_deltas: vec![], ..ParamGrid::small() }, Objective::Cagr, None);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].exposure_ok && rows[0].exposure_shortfall > 0.15, "{}", rows[0].exposure_shortfall);
        assert!(rows[0].label().contains("實際"));
    }

    #[test]
    fn bootstrap_deterministic() {
        let data = gbm_data(6.0, 0.08, 0.2, 20.0, 0.02, 0.01, 4);
        let cfg = RollConfig::default();
        let a = bootstrap_robustness(&data, &cfg, 24, 20.0, 42).unwrap();
        let b = bootstrap_robustness(&data, &cfg, 24, 20.0, 42).unwrap();
        let c = bootstrap_robustness(&data, &cfg, 24, 20.0, 43).unwrap();
        assert_eq!(a, b);
        assert_ne!(a.cagr_samples, c.cagr_samples);
        assert_eq!(a.n_samples, 24);
        assert!(a.cagr.p5 <= a.cagr.p50 && a.cagr.p50 <= a.cagr.p95);
        assert!((0.0..=1.0).contains(&a.prob_beat_benchmark));
    }

    #[test]
    fn walk_forward_windows() {
        let data = gbm_data(10.0, 0.08, 0.2, 20.0, 0.02, 0.01, 8);
        let grid = ParamGrid { deltas: vec![0.7, 0.85], entry_dtes: vec![540], roll_dtes: vec![180], exposures: vec![1.0, 1.5], ..ParamGrid::small() };
        let w = walk_forward(&data, &RollConfig::default(), &grid, Objective::Cagr, 4.0, 2.0).unwrap();
        assert!(w.len() >= 2, "{}", w.len());
        for x in &w {
            assert!(x.test_start > x.train_end);
            assert_eq!(x.best_config.start, Some(x.test_start));
        }
        let s = walk_forward_summary(&w);
        assert!(s.years > 3.0);
    }

    #[test]
    fn sensitivity_and_heatmap_shapes() {
        let data = gbm_data(5.0, 0.08, 0.2, 20.0, 0.02, 0.01, 6);
        let base = RollConfig::default();
        let s = sensitivity(&data, &base, SweepParam::Exposure, &[1.0, 1.5, 2.0]);
        assert_eq!(s.len(), 3);
        // more exposure ⇒ more volatility
        let mut s = s;
        s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert!(s[0].1.ann_vol < s[2].1.ann_vol);
        let h = heatmap(&data, &base, &[0.7, 0.8], &[365, 540, 730], Objective::Cagr);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].len(), 3);
        assert!(h.iter().flatten().all(|v| v.is_finite()));
    }
}
