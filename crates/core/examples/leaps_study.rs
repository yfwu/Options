//! LEAPS roll study on the bundled historical data.
//!
//! ```text
//! cargo run --release -p optlab-core --example leaps_study [-- --quick]
//! ```
//!
//! Data (in `<workspace>/data`):
//! * `QQQ_adj_1999.csv` (Date,adj_close) + `VXN_History.csv` (from 2009-09) + `VIX_History.csv`.
//!   Before VXN exists the QQQ vol index is a **proxy**:
//!   `max(VIX × ratio, 1.10 × RV21(QQQ))`, where `ratio` is the median VXN/VIX
//!   over the overlap (≈1.2–1.3). The realised-vol floor matters in 2000–2002, when
//!   Nasdaq vol was ~2× SPX vol and a plain VIX ratio would make QQQ LEAPS far too cheap.
//! * `VIX_SPY_1993.csv` (Date,VIX,SPY adj close) for SPY.
//! * Rates: built-in approximate 3M T-bill history (`approx_us_tbill_rates`).
//!
//! The CSV parsing here is deliberately self-contained (no optlab-data dependency).

use chrono::NaiveDate;
use optlab_core::backtest::{approx_us_tbill_rates, run_backtest, BacktestData, BacktestResult, RateSource, RollConfig, StrikeSelection};
use optlab_core::chain::OptionKind;
use optlab_core::history::{Bar, PriceSeries, ScalarSeries};
use optlab_core::optimize::{bootstrap_robustness, optimize, sensitivity, walk_forward, walk_forward_summary, Objective, ParamGrid, SweepParam};
use optlab_core::stats::PerformanceStats;
use optlab_core::synth::SynthVolModel;
use std::path::PathBuf;
use std::time::Instant;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data")
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().or_else(|| NaiveDate::parse_from_str(s, "%m/%d/%Y").ok())
}

/// Read (date, column) pairs from a simple CSV with a header.
fn read_col(file: &str, col: usize) -> Vec<(NaiveDate, f64)> {
    let txt = std::fs::read_to_string(data_dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((parse_date(f.first()?)?, f.get(col)?.trim().parse::<f64>().ok()?))
        })
        .filter(|p| p.1.is_finite() && p.1 > 0.0)
        .collect()
}

fn series_from(name: &str, pts: &[(NaiveDate, f64)]) -> PriceSeries {
    let bars = pts.iter().map(|&(d, p)| Bar { date: d, open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 }).collect();
    PriceSeries::new(name, bars)
}

fn qqq_data() -> (BacktestData, f64) {
    let qqq = read_col("QQQ_adj_1999.csv", 1);
    let vxn = ScalarSeries::new("VXN", read_col("VXN_History.csv", 4));
    let vix = ScalarSeries::new("VIX", read_col("VIX_History.csv", 4));
    // overlap ratio
    let mut ratios: Vec<f64> = vxn.points.iter().filter_map(|&(d, v)| vix.value_at(d).map(|x| v / x)).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ratio = ratios[ratios.len() / 2];
    let first_vxn = vxn.points[0].0;
    // RV21 of QQQ
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
        let v = vix.value_at(d).unwrap_or(20.0) * ratio;
        pts.push((d, v.max(1.10 * rv)));
    }
    pts.extend(vxn.points.iter().copied());
    let vol = ScalarSeries::new("VXN (VIX/RV proxy before 2009-09)", pts);
    (BacktestData::new(series_from("QQQ", &qqq), vol, RateSource::Series(approx_us_tbill_rates()), 0.007), ratio)
}

fn spy_data() -> BacktestData {
    let spy = read_col("VIX_SPY_1993.csv", 2);
    let vix = ScalarSeries::new("VIX", read_col("VIX_SPY_1993.csv", 1));
    BacktestData::new(series_from("SPY", &spy), vix, RateSource::Series(approx_us_tbill_rates()), 0.017)
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
        "  [{}] {} → {} ({:.1}y) | rolls {} | costs ${:.0} (drag {:.2}%/yr of equity) | avg exposure {:.2}x | entry extrinsic {:.2}%/yr of spot, {:.2}%/yr of Δ-notional | theta {:.2}%/yr of equity | avg rf {:.2}% | {:.2} ms{}",
        r.config.label(),
        r.stats.start,
        r.stats.end,
        r.stats.years,
        r.n_rolls,
        r.total_costs,
        r.cost_drag_annual * 100.0,
        r.avg_exposure,
        r.avg_extrinsic_pct_annual * 100.0,
        r.avg_carry_pct_of_exposure * 100.0,
        r.avg_theta_pct_equity * 100.0,
        r.avg_rate * 100.0,
        ms,
        if r.wiped_out { format!(" | WIPED OUT {}", r.wiped_date.unwrap()) } else { String::new() }
    );
    line("LEAPS roll", &r.stats);
    for (n, s) in &r.benchmark_stats {
        line(n, s);
    }
    let yr = |s: &PerformanceStats, y: i32| s.yearly.iter().find(|p| p.0 == y).map_or("     -".to_string(), |p| format!("{:>6.1}", p.1 * 100.0));
    print!("  year:      ");
    let years = [2000, 2001, 2002, 2008, 2009, 2013, 2018, 2020, 2022, 2023, 2024];
    for y in years {
        print!("{y:>7}");
    }
    println!();
    for (n, s) in std::iter::once(("LEAPS", &r.stats)).chain(r.benchmark_stats.iter().map(|(n, s)| (n.as_str(), s))) {
        let short: String = n.chars().take(10).collect();
        print!("  {short:<10} ");
        for y in years {
            print!(" {}", yr(s, y));
        }
        println!();
    }
}

fn pricing_sanity() {
    println!("\n=== 1. Synthetic LEAPS pricing sanity (0.80Δ, 540 DTE, S=100) ===");
    println!("  model  vol-idx  r     K/S    IV     price%S  extrinsic%S/yr  carry%Δnotional/yr");
    for (name, m, q) in [("SPY", SynthVolModel::spy(), 0.013), ("QQQ", SynthVolModel::qqq(), 0.006)] {
        for &(vix, r) in &[(12.0, 0.0), (12.0, 0.045), (17.0, 0.0), (17.0, 0.02), (17.0, 0.045), (25.0, 0.045), (40.0, 0.045), (60.0, 0.02)] {
            let (s, t) = (100.0, 540.0 / 365.0);
            let k = m.strike_for_delta(OptionKind::Call, 0.8, s, t, r, q, vix);
            let (g, iv) = m.greeks(OptionKind::Call, s, k, t, r, q, vix);
            let ext = g.price - (s - k).max(0.0);
            println!(
                "  {name}   {vix:>5.0}  {:>4.1}%  {:.3}  {:>5.1}%  {:>6.1}%  {:>8.2}%  {:>12.2}%",
                r * 100.0,
                k / s,
                iv * 100.0,
                g.price,
                ext / s / t * 100.0,
                ext / (g.delta * s) / t * 100.0
            );
        }
    }
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    pricing_sanity();

    let (qqq, ratio) = qqq_data();
    let spy = spy_data();
    println!("\nQQQ vol proxy: VXN/VIX median ratio on overlap = {ratio:.3}; before 2009-09 vol = max(VIX×{ratio:.2}, 1.1×RV21)");

    println!("\n=== 2. Default stock-replacement backtests ===");
    let mut defaults = Vec::new();
    for (data, cfg) in [(&qqq, RollConfig::qqq()), (&spy, RollConfig::spy())] {
        let t0 = Instant::now();
        let r = run_backtest(data, &cfg).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1e3;
        println!("\n{}:", data.prices.symbol);
        report(&r, ms);
        defaults.push(r);
        // same config without rebalancing band and at 1.0x to see pure carry drag
        for c in [
            RollConfig { target_exposure: 1.0, ..cfg.clone() },
            RollConfig { target_exposure: 2.0, ..cfg.clone() },
            RollConfig { rebalance_band: None, ..cfg.clone() },
        ] {
            let r = run_backtest(data, &c).unwrap();
            println!("  variant {}:", c.label());
            line("LEAPS roll", &r.stats);
            line(&r.benchmark_stats[1].0, &r.benchmark_stats[1].1);
        }
        // post-2009 sub-period (real VXN for QQQ)
        let c = RollConfig { start: NaiveDate::from_ymd_opt(2009, 10, 1), ..cfg.clone() };
        let r = run_backtest(data, &c).unwrap();
        println!("  sub-period from 2009-10 ({}):", c.label());
        line("LEAPS roll", &r.stats);
        for (n, s) in &r.benchmark_stats {
            line(n, s);
        }
    }

    // Timing: repeated runs
    let t0 = Instant::now();
    let n_rep = 50;
    for _ in 0..n_rep {
        let _ = run_backtest(&spy, &RollConfig::spy()).unwrap();
    }
    println!("\nTiming: run_backtest SPY {:.1}y full (with records & benchmarks) = {:.2} ms/run", defaults[1].stats.years, t0.elapsed().as_secs_f64() * 1e3 / n_rep as f64);

    println!("\n=== 3. Grid optimisation (default grid) ===");
    let grid = ParamGrid::default();
    let mut bests = Vec::new();
    for (data, base) in [(&qqq, RollConfig::qqq()), (&spy, RollConfig::spy())] {
        for obj in [Objective::Utility { lambda: 0.25 }, Objective::Calmar, Objective::MaxCagrWithDdCap { max_dd: 0.55 }] {
            let t0 = Instant::now();
            let rows = optimize(data, &base, &grid, obj, None);
            let el = t0.elapsed().as_secs_f64();
            println!("\n{} — objective {} — {} combos in {:.2}s ({:.2} ms/run on all cores)", data.prices.symbol, obj.label(), rows.len(), el, el * 1e3 / rows.len() as f64);
            for r in rows.iter().take(6) {
                println!(
                    "  score {:>7.3} | {:<32} CAGR {} MDD {} Sharpe {:>5.2} rolls {:>3} wiped {}",
                    r.score,
                    r.config.label(),
                    pct(r.stats.cagr),
                    pct(r.stats.max_drawdown),
                    r.stats.sharpe,
                    r.n_rolls,
                    r.wiped_out
                );
            }
            let wiped = rows.iter().filter(|r| r.wiped_out).count();
            println!("  ({wiped} of {} combos wiped out)", rows.len());
            if matches!(obj, Objective::Utility { .. }) {
                bests.push((data, rows[0].config.clone()));
            }
        }
    }

    println!("\n=== 4. Sensitivity (exposure & delta), SPY then QQQ ===");
    for (data, base) in [(&spy, RollConfig::spy()), (&qqq, RollConfig::qqq())] {
        let mut s = sensitivity(data, &base, SweepParam::Exposure, &[0.5, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0]);
        s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        println!("{} exposure sweep:", data.prices.symbol);
        for (v, st) in &s {
            println!("  {v:>4.2}x  CAGR {} MDD {} vol {} Sharpe {:>5.2}", pct(st.cagr), pct(st.max_drawdown), pct(st.ann_vol), st.sharpe);
        }
        let mut s = sensitivity(data, &base, SweepParam::Delta, &[0.5, 0.6, 0.7, 0.8, 0.9, 0.95]);
        s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        println!("{} delta sweep @1.5x:", data.prices.symbol);
        for (v, st) in &s {
            println!("  Δ{v:>4.2}  CAGR {} MDD {} vol {} Sharpe {:>5.2}", pct(st.cagr), pct(st.max_drawdown), pct(st.ann_vol), st.sharpe);
        }
        let mut s = sensitivity(data, &base, SweepParam::HalfSpreadPct, &[0.0, 0.0025, 0.005, 0.01, 0.02]);
        s.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        println!("{} half-spread sweep:", data.prices.symbol);
        for (v, st) in &s {
            println!("  {:>4.2}%  CAGR {}", v * 100.0, pct(st.cagr));
        }
    }

    println!("\n=== 5. Walk-forward (train 8y / test 3y, small grid, Utility λ=0.25) ===");
    for (data, base) in [(&spy, RollConfig::spy()), (&qqq, RollConfig::qqq())] {
        let w = walk_forward(data, &base, &ParamGrid::small(), Objective::Utility { lambda: 0.25 }, 8.0, 3.0).unwrap();
        println!("{}:", data.prices.symbol);
        for x in &w {
            println!(
                "  test {}..{}  best {:<26} IS {:>6.3} | OOS CAGR {} MDD {} | base CAGR {} MDD {} | B&H CAGR {}",
                x.test_start,
                x.test_end,
                x.best_config.label(),
                x.in_sample_score,
                pct(x.oos_stats.cagr),
                pct(x.oos_stats.max_drawdown),
                pct(x.base_oos_stats.cagr),
                pct(x.base_oos_stats.max_drawdown),
                pct(x.bh_oos_stats.cagr)
            );
        }
        let s = walk_forward_summary(&w);
        println!(
            "  chained OOS {:.1}y: optimised {} | default {} | B&H {} | opt beats default in {:.0}% of windows",
            s.years,
            pct(s.oos_cagr),
            pct(s.base_cagr),
            pct(s.bh_cagr),
            s.pct_windows_opt_beats_base * 100.0
        );
    }

    println!("\n=== 6. Block bootstrap robustness (joint return/vol blocks, mean block 60d) ===");
    let n = if quick { 100 } else { 400 };
    let mut cases: Vec<(&BacktestData, RollConfig, &str)> = vec![
        (&spy, RollConfig::spy(), "SPY default"),
        (&qqq, RollConfig::qqq(), "QQQ default"),
        (&qqq, RollConfig { start: NaiveDate::from_ymd_opt(2009, 10, 1), ..RollConfig::qqq() }, "QQQ default, 2009+ only"),
        (&spy, RollConfig { target_exposure: 1.0, ..RollConfig::spy() }, "SPY 1.0x"),
        (&qqq, RollConfig { target_exposure: 2.5, ..RollConfig::qqq() }, "QQQ 2.5x"),
    ];
    for (d, c) in &bests {
        cases.push((d, c.clone(), if d.prices.symbol == "SPY" { "SPY optimiser best" } else { "QQQ optimiser best" }));
    }
    for (data, cfg, name) in cases {
        let t0 = Instant::now();
        let r = bootstrap_robustness(data, &cfg, n, 60.0, 7).unwrap();
        println!(
            "  {name:<24} [{}] n={} {:.0}s\n     CAGR p5/p50/p95 {}/{}/{}  MDD p5/p50 {}/{} | B&H CAGR p50 {} MDD p50 {} | {:.2}x-ETF CAGR p50 {} | P(wipe) {:.1}% P(beat B&H) {:.0}% P(beat LETF) {:.0}% P(loss) {:.1}% P(DD>50%) {:.0}%",
            cfg.label(),
            r.n_samples,
            t0.elapsed().as_secs_f64(),
            pct(r.cagr.p5).trim(),
            pct(r.cagr.p50).trim(),
            pct(r.cagr.p95).trim(),
            pct(r.max_dd.p5).trim(),
            pct(r.max_dd.p50).trim(),
            pct(r.bh_cagr.p50).trim(),
            pct(r.bh_max_dd.p50).trim(),
            cfg.target_exposure,
            pct(r.lev_cagr.p50).trim(),
            r.prob_wipeout * 100.0,
            r.prob_beat_benchmark * 100.0,
            r.prob_beat_lev_etf * 100.0,
            r.prob_loss * 100.0,
            r.prob_dd_over_50 * 100.0
        );
    }
    let _ = StrikeSelection::Delta(0.8);
}
