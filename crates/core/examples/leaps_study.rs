//! LEAPS roll study on the bundled historical data (with before/after attribution
//! of the engine-validation fixes).
//!
//! ```text
//! cargo run --release -p optlab-core --example leaps_study [-- --quick] [-- --advise] [-- --kb]
//! ```
//!
//! Data (in `<workspace>/data`), parsed here without the optlab-data crate:
//! * QQQ: `QQQ_adj_1999.csv` (total-return adj close). Vol index = CBOE `VXN_History.csv`
//!   (2009-09 →), Barchart `VXN_barchart_2000.csv` (2000-10-10 → 2009-09) and, before
//!   that, `VIX × 2.13` (the 2000–2002 VXN/VIX ratio; same rule as `optlab_data::presets`).
//! * SPY: `VIX_SPY_1993.csv` (VIX, SPY adj close).
//! * Rates: FRED `DGS3MO.csv` (3M T-bill, percent).
//! * "Legacy" inputs (for attribution only): the old QQQ proxy
//!   `max(VIX × median VXN/VIX, 1.1 × RV21)` before 2009-09 and the approximate T-bill table.

use chrono::NaiveDate;
use optlab_core::advisor::{advise, strategy_conclusions, MarketSnapshot};
use optlab_core::backtest::{approx_us_tbill_rates, run_backtest, BacktestData, BacktestResult, RateSource, RollConfig, StrikeSelection};
use optlab_core::chain::OptionKind;
use optlab_core::history::{Bar, PriceSeries, ScalarSeries};
use optlab_core::optimize::{
    bootstrap_robustness, optimize, optimize_robust, sensitivity, walk_forward, walk_forward_summary, Objective, OptRow, ParamGrid, RobustOptions,
    SweepParam,
};
use optlab_core::stats::PerformanceStats;
use optlab_core::synth::{CostModel, SynthVolModel, TermModel};
use std::path::PathBuf;
use std::time::Instant;

/// VXN/VIX ratio used before Barchart VXN starts (2000-10-10), as in optlab-data.
const VXN_VIX_EARLY: f64 = 2.13;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data")
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().or_else(|| NaiveDate::parse_from_str(s, "%m/%d/%Y").ok())
}

/// Read (date, column) pairs from a simple CSV with a header; keeps values ≥ `min`.
fn read_col_min(file: &str, col: usize, min: f64) -> Vec<(NaiveDate, f64)> {
    let txt = std::fs::read_to_string(data_dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((parse_date(f.first()?)?, f.get(col)?.trim().parse::<f64>().ok()?))
        })
        .filter(|p| p.1.is_finite() && p.1 >= min)
        .collect()
}

fn read_col(file: &str, col: usize) -> Vec<(NaiveDate, f64)> {
    read_col_min(file, col, 1e-12)
}

fn series_from(name: &str, pts: &[(NaiveDate, f64)]) -> PriceSeries {
    let bars = pts.iter().map(|&(d, p)| Bar { date: d, open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 }).collect();
    PriceSeries::new(name, bars)
}

fn dgs3mo() -> RateSource {
    // zero yields (2011–2015, 2020–21) are real values: keep them
    RateSource::Series(ScalarSeries::new("DGS3MO", read_col_min("DGS3MO.csv", 1, 0.0)))
}

/// QQQ with real VXN (CBOE + Barchart) and DGS3MO rates.
fn qqq_data() -> BacktestData {
    let qqq = read_col("QQQ_adj_1999.csv", 1);
    let cboe = read_col("VXN_History.csv", 4);
    let barchart = read_col("VXN_barchart_2000.csv", 4);
    let vix = ScalarSeries::new("VIX", read_col("VIX_History.csv", 4));
    let (bc0, cb0) = (barchart[0].0, cboe[0].0);
    let mut pts: Vec<(NaiveDate, f64)> = qqq.iter().filter(|p| p.0 < bc0).map(|&(d, _)| (d, vix.value_at(d).unwrap_or(25.0) * VXN_VIX_EARLY)).collect();
    pts.extend(barchart.iter().copied().filter(|p| p.0 < cb0));
    pts.extend(cboe.iter().copied());
    BacktestData::new(series_from("QQQ", &qqq), ScalarSeries::new("VXN", pts), dgs3mo(), 0.006)
}

/// Legacy QQQ inputs (old proxy before 2009-09, approximate rate table, q = 0.7%).
fn qqq_data_legacy() -> BacktestData {
    let qqq = read_col("QQQ_adj_1999.csv", 1);
    let vxn = ScalarSeries::new("VXN", read_col("VXN_History.csv", 4));
    let vix = ScalarSeries::new("VIX", read_col("VIX_History.csv", 4));
    let mut ratios: Vec<f64> = vxn.points.iter().filter_map(|&(d, v)| vix.value_at(d).map(|x| v / x)).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ratio = ratios[ratios.len() / 2];
    let first_vxn = vxn.points[0].0;
    let lr: Vec<f64> = qqq.windows(2).map(|w| (w[1].1 / w[0].1).ln()).collect();
    let mut pts = Vec::new();
    for (i, &(d, _)) in qqq.iter().enumerate() {
        if d >= first_vxn {
            break;
        }
        let rv = if i >= 21 {
            let w = &lr[i - 21..i];
            let m = w.iter().sum::<f64>() / 21.0;
            (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / 20.0 * 252.0).sqrt() * 100.0
        } else {
            0.0
        };
        pts.push((d, (vix.value_at(d).unwrap_or(20.0) * ratio).max(1.10 * rv)));
    }
    pts.extend(vxn.points.iter().copied());
    BacktestData::new(series_from("QQQ", &qqq), ScalarSeries::new("VXN proxy", pts), RateSource::Series(approx_us_tbill_rates()), 0.007)
}

fn spy_data() -> BacktestData {
    let spy = read_col("VIX_SPY_1993.csv", 2);
    let vix = ScalarSeries::new("VIX", read_col("VIX_SPY_1993.csv", 1));
    BacktestData::new(series_from("SPY", &spy), vix, dgs3mo(), 0.014)
}

fn spy_data_legacy() -> BacktestData {
    BacktestData { rate: RateSource::Series(approx_us_tbill_rates()), dividend_yield: 0.017, ..spy_data() }
}

/// Engine behaviour before the validation fixes: κ = 1 term structure, no crisis
/// spread widening, same-close execution, old half spreads (SPY 0.5%, QQQ 0.6%).
fn legacy(mut c: RollConfig, old_half_spread: f64) -> RollConfig {
    c.vol_model = c.vol_model.with_term_model(TermModel::SingleKappa);
    c.costs = c.costs.without_crisis_widening();
    c.costs.half_spread_pct = old_half_spread;
    c.exec_lag_days = 0;
    c
}

fn pct(x: f64) -> String {
    format!("{:>7.1}%", x * 100.0)
}

fn line(name: &str, s: &PerformanceStats) {
    println!(
        "  {:<34} CAGR {} vol {} Sharpe {:>5.2} MDD {} Calmar {:>5.2} worst yr {} final x{:.1}",
        name,
        pct(s.cagr),
        pct(s.ann_vol),
        s.sharpe,
        pct(s.max_drawdown),
        s.calmar,
        s.worst_year.map_or("-".into(), |(y, r)| format!("{y} {}", pct(r).trim())),
        s.final_value / s.initial_value.max(1e-9)
    );
}

fn report(r: &BacktestResult, ms: f64) {
    println!(
        "  [{}] {} → {} ({:.1}y) | rolls {} | cost drag {:.2}%/yr | avg exposure {:.2}x | min equity {:.1}%{} | entry extrinsic {:.2}%/yr of spot | {:.2} ms",
        r.label(),
        r.stats.start,
        r.stats.end,
        r.stats.years,
        r.n_rolls,
        r.cost_drag_annual * 100.0,
        r.avg_exposure,
        r.min_equity_ratio * 100.0,
        if r.near_wipe { " NEAR-WIPE" } else { "" },
        r.avg_extrinsic_pct_annual * 100.0,
        ms,
    );
    line("LEAPS roll", &r.stats);
    for (n, s) in &r.benchmark_stats {
        line(n, s);
    }
    println!(
        "  edge: vs B&H {:+.2}%/yr | vs daily L× {:+.2}%/yr | vs exposure-matched linear {:+.2}%/yr (option-specific)",
        (r.stats.cagr - r.benchmark_stats[0].1.cagr) * 100.0,
        (r.stats.cagr - r.benchmark_stats[1].1.cagr) * 100.0,
        (r.stats.cagr - r.benchmark_stats[2].1.cagr) * 100.0
    );
}

fn short(r: &BacktestResult) -> String {
    format!(
        "CAGR {} MDD {} minEq {:>5.1}% avgExp {:.2}x | vsB&H {:+.2} vsMatched {:+.2}",
        pct(r.stats.cagr),
        pct(r.stats.max_drawdown),
        r.min_equity_ratio * 100.0,
        r.avg_exposure,
        (r.stats.cagr - r.benchmark_stats[0].1.cagr) * 100.0,
        (r.stats.cagr - r.benchmark_stats[2].1.cagr) * 100.0
    )
}

fn pricing_sanity() {
    println!("\n=== 1. Synthetic pricing sanity: ATM term structure (two-factor vs legacy κ=1) and 0.80Δ 540-DTE carry ===");
    println!("  model  vol-idx | ATM 1m   3m    1y   1.5y | legacy 1y 1.5y | 0.80Δ K/S  IV    price%S  extr%S/yr (r=4%)");
    for (name, m, q) in [("SPY", SynthVolModel::spy(), 0.013), ("QQQ", SynthVolModel::qqq(), 0.006)] {
        let old = m.with_term_model(TermModel::SingleKappa);
        for &v in &[12.0, 17.0, 25.0, 40.0, 60.0, 82.7] {
            let (s, t, r) = (100.0, 540.0 / 365.0, 0.043);
            let k = m.strike_for_delta(OptionKind::Call, 0.8, s, t, r, q, v);
            let (g, iv) = m.greeks(OptionKind::Call, s, k, t, r, q, v);
            let ext = g.price - (s - k).max(0.0);
            println!(
                "  {name}   {v:>6.1}  | {:>5.1} {:>5.1} {:>5.1} {:>5.1} | {:>5.1} {:>5.1}   | {:.3} {:>5.1}% {:>6.1}%  {:>6.2}%",
                m.atm_vol(1.0 / 12.0, v) * 100.0,
                m.atm_vol(0.25, v) * 100.0,
                m.atm_vol(1.0, v) * 100.0,
                m.atm_vol(1.5, v) * 100.0,
                old.atm_vol(1.0, v) * 100.0,
                old.atm_vol(1.5, v) * 100.0,
                k / s,
                iv * 100.0,
                g.price,
                ext / s / t * 100.0
            );
        }
    }
}

fn opt_line(r: &OptRow) -> String {
    format!(
        "score {:>7.3} (hist {:>7.3}, IQR {:.3}) | {:<44} CAGR {} MDD {} minEq {:>5.1}%{}",
        r.score,
        r.history_score,
        r.score_iqr,
        r.label(),
        pct(r.stats.cagr),
        pct(r.stats.max_drawdown),
        r.min_equity_ratio * 100.0,
        if r.exposure_ok { "" } else { " (exposure short)" }
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|a| a == "--quick");
    pricing_sanity();

    let qqq = qqq_data();
    let spy = spy_data();
    let qqq_old = qqq_data_legacy();
    let spy_old = spy_data_legacy();

    println!("\n=== 2. Current engine (two-factor IV, crisis spreads, exec lag 1, real VXN + DGS3MO) ===");
    for (data, base) in [(&qqq, RollConfig::qqq()), (&spy, RollConfig::spy())] {
        println!("\n{}:", data.prices.symbol);
        for e in [1.0, 1.5, 2.0] {
            let c = RollConfig { target_exposure: e, ..base.clone() };
            let t0 = Instant::now();
            let r = run_backtest(data, &c).unwrap();
            report(&r, t0.elapsed().as_secs_f64() * 1e3);
        }
        let c = RollConfig { target_exposure: 1.5, start: NaiveDate::from_ymd_opt(2009, 10, 1), ..base.clone() };
        let r = run_backtest(data, &c).unwrap();
        println!("  from 2009-10, 1.5x: {}", short(&r));
    }

    println!("\n=== 3. Before/after attribution (1.0x and 1.5x, Δ0.80 540/180 ±25%) ===");
    for (sym, data_new, data_old, base, old_hs) in [("QQQ", &qqq, &qqq_old, RollConfig::qqq(), 0.006), ("SPY", &spy, &spy_old, RollConfig::spy(), 0.005)] {
        for e in [1.0, 1.5, 2.0] {
            let b = RollConfig { target_exposure: e, ..base.clone() };
            let l = legacy(b.clone(), old_hs);
            let rows: Vec<(&str, &BacktestData, RollConfig)> = vec![
                ("legacy engine + legacy data (original)", data_old, l.clone()),
                ("legacy engine + real VXN/DGS3MO (B3/B5)", data_new, l.clone()),
                ("  + two-factor term structure (B2)", data_new, RollConfig { vol_model: b.vol_model, ..l.clone() }),
                ("  + crisis spreads & new half spread (B4)", data_new, RollConfig { vol_model: b.vol_model, costs: b.costs, ..l.clone() }),
                ("  + exec lag 1 = current engine (B7)", data_new, b.clone()),
                ("current, CostModel::conservative()", data_new, RollConfig { costs: CostModel::conservative(), ..b.clone() }),
            ];
            println!("{sym} {e:.1}x:");
            for (name, d, c) in rows {
                let r = run_backtest(d, &c).unwrap();
                println!("  {name:<44} {}", short(&r));
            }
        }
    }

    let t0 = Instant::now();
    let n_rep = 30;
    for _ in 0..n_rep {
        let _ = run_backtest(&spy, &RollConfig::spy()).unwrap();
    }
    println!("\nTiming: run_backtest SPY 33y with records & 3 benchmarks = {:.2} ms/run", t0.elapsed().as_secs_f64() * 1e3 / n_rep as f64);

    println!("\n=== 4. Optimisation: single-history argmax vs robust (median over 12 bootstrap paths + history, neighbour-smoothed) ===");
    let grid = ParamGrid::default();
    let mut bests: Vec<(&BacktestData, RollConfig, String)> = Vec::new();
    for (data, base) in [(&qqq, RollConfig::qqq()), (&spy, RollConfig::spy())] {
        for obj in [Objective::Utility { lambda: 0.25 }, Objective::Calmar] {
            let t0 = Instant::now();
            let rows = optimize(data, &base, &grid, obj, None);
            let t_plain = t0.elapsed().as_secs_f64();
            let t0 = Instant::now();
            let robust = optimize_robust(data, &base, &grid, obj, &RobustOptions::default(), None).unwrap();
            let t_rob = t0.elapsed().as_secs_f64();
            println!("\n{} — {} — {} combos (plain {:.2}s, robust {:.1}s)", data.prices.symbol, obj.label(), rows.len(), t_plain, t_rob);
            println!("  single-history top 3:");
            for r in rows.iter().take(3) {
                println!("    {}", opt_line(r));
            }
            println!("  robust top 3:");
            for r in robust.iter().take(3) {
                println!("    {}", opt_line(r));
            }
            let nw = rows.iter().filter(|r| r.near_wipe).count();
            let short_n = rows.iter().filter(|r| !r.exposure_ok).count();
            println!("  ({nw} of {} combos near-wipe (<10% min equity), {short_n} under-exposed)", rows.len());
            if matches!(obj, Objective::Utility { .. }) {
                bests.push((data, robust[0].config.clone(), format!("{} robust best", data.prices.symbol)));
            }
        }
    }

    println!("\n=== 5. Sensitivity (current engine) ===");
    for (data, base) in [(&spy, RollConfig::spy()), (&qqq, RollConfig::qqq())] {
        let mut s = sensitivity(data, &base, SweepParam::Exposure, &[0.5, 1.0, 1.25, 1.5, 2.0, 2.5, 3.0]);
        s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        println!("{} exposure sweep:", data.prices.symbol);
        for (v, st) in &s {
            println!("  {v:>4.2}x  CAGR {} MDD {} vol {} Sharpe {:>5.2}", pct(st.cagr), pct(st.max_drawdown), pct(st.ann_vol), st.sharpe);
        }
        let mut s = sensitivity(data, &base, SweepParam::Delta, &[0.6, 0.7, 0.8, 0.9]);
        s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        print!("{} delta sweep @1.5x:", data.prices.symbol);
        for (v, st) in &s {
            print!("  Δ{v:.1} {}", pct(st.cagr).trim());
        }
        println!();
    }

    println!("\n=== 6. Walk-forward (train 8y / test 3y, small grid, Utility λ=0.25) ===");
    for (data, base) in [(&spy, RollConfig::spy()), (&qqq, RollConfig::qqq())] {
        let w = walk_forward(data, &base, &ParamGrid::small(), Objective::Utility { lambda: 0.25 }, 8.0, 3.0).unwrap();
        let s = walk_forward_summary(&w);
        println!(
            "  {}: chained OOS {:.1}y: optimised {} | default {} | B&H {} | opt beats default in {:.0}% of {} windows",
            data.prices.symbol,
            s.years,
            pct(s.oos_cagr),
            pct(s.base_cagr),
            pct(s.bh_cagr),
            s.pct_windows_opt_beats_base * 100.0,
            w.len()
        );
    }

    println!("\n=== 7. Block bootstrap (joint return/vol blocks, mean block 60d) ===");
    let n = if quick { 100 } else { 300 };
    let mut cases: Vec<(&BacktestData, RollConfig, String)> = Vec::new();
    for (data, base) in [(&spy, RollConfig::spy()), (&qqq, RollConfig::qqq())] {
        for e in [1.0, 1.5, 2.0] {
            cases.push((data, RollConfig { target_exposure: e, ..base.clone() }, format!("{} {e:.1}x", data.prices.symbol)));
        }
    }
    cases.extend(bests);
    for (data, cfg, name) in cases {
        let r = bootstrap_robustness(data, &cfg, n, 60.0, 7).unwrap();
        println!(
            "  {name:<18} CAGR p5/p50/p95 {}/{}/{} MDD p50 {} | minEq p5 {} | P(near-wipe<10%) {:.1}% P(wipe) {:.1}% | B&H p50 {} | matched p50 {} | P(beat B&H) {:.0}% P(beat matched) {:.0}% P(beat LETF) {:.0}%",
            pct(r.cagr.p5).trim(),
            pct(r.cagr.p50).trim(),
            pct(r.cagr.p95).trim(),
            pct(r.max_dd.p50).trim(),
            pct(r.min_equity.p5).trim(),
            r.prob_near_wipe * 100.0,
            r.prob_wipeout * 100.0,
            pct(r.bh_cagr.p50).trim(),
            pct(r.matched_cagr.p50).trim(),
            r.prob_beat_benchmark * 100.0,
            r.prob_beat_matched * 100.0,
            r.prob_beat_lev_etf * 100.0,
        );
    }

    if args.iter().any(|a| a == "--advise") {
        println!("\n=== 8. Advisor (current snapshot) ===");
        for (data, base) in [(&qqq, RollConfig::qqq()), (&spy, RollConfig::spy())] {
            let snap = MarketSnapshot::from_history(&data.prices, &data.vol_index);
            let rows = optimize(data, &base, &ParamGrid::small(), Objective::Utility { lambda: 0.25 }, None);
            let current = RollConfig { strike: StrikeSelection::Delta(0.8), ..base.clone() };
            for a in advise(&snap, rows.first(), Some(&current)) {
                println!("[{}] {}\n    {}", a.level.label(), a.title, a.body);
            }
        }
    }
    if args.iter().any(|a| a == "--kb") {
        for a in strategy_conclusions() {
            println!("[{}] {}\n    {}", a.level.label(), a.title, a.body);
        }
    }
}
