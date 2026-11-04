//! Minimal reproductions for docs/validation/REPORT.md §4. Run:
//!   cargo run --release --bin repro
use chrono::{Duration, NaiveDate};
use optlab_core::backtest::{run_backtest, BacktestData, RateSource, RollConfig, StrikeSelection};
use optlab_core::history::{Bar, PriceSeries, ScalarSeries};
use optlab_core::synth::SynthVolModel;

/// deterministic path: flat 20 days, then -85% linearly over 2 years, then flat
fn crash_path() -> BacktestData {
    let mut d = NaiveDate::from_ymd_opt(2000, 1, 3).unwrap();
    let (mut bars, mut vols) = (vec![], vec![]);
    let n = 20 + 504 + 250;
    for i in 0..n {
        let p = if i < 20 { 100.0 } else if i < 524 { 100.0 * (1.0 - 0.85 * (i - 20) as f64 / 504.0) } else { 15.0 };
        let v = if (20..524).contains(&i) { 45.0 } else { 25.0 };
        bars.push(Bar { date: d, open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 });
        vols.push((d, v));
        d += Duration::days(if d.format("%a").to_string() == "Fri" { 3 } else { 1 });
    }
    BacktestData::new(PriceSeries::new("CRASH", bars), ScalarSeries::new("VOL", vols), RateSource::Constant(0.03), 0.0)
}

fn main() {
    let data = crash_path();
    println!("R1  wipe-out flag vs real loss (−85% linear crash, 0.80Δ 540/180):");
    for e in [1.0, 2.0, 3.0] {
        let r = run_backtest(&data, &RollConfig { target_exposure: e, ..RollConfig::qqq() }).unwrap();
        let min = r.records.iter().map(|x| x.equity).fold(f64::INFINITY, f64::min) / 1e5;
        println!("    {e:.1}x  wiped_out={}  min equity = {:.2}% of initial  MDD = {:.1}%", r.wiped_out, min * 100.0, r.stats.max_drawdown * 100.0);
    }
    println!("R2  1.00x 'leveraged' benchmark is charged the expense ratio:");
    let r = run_backtest(&data, &RollConfig { target_exposure: 1.0, ..RollConfig::qqq() }).unwrap();
    println!("    B&H final {:.4}  vs  '每日再平衡 1.00x' final {:.4}  (should be equal)", r.benchmark_stats[0].1.final_value / 1e5, r.benchmark_stats[1].1.final_value / 1e5);
    println!("R3  long-dated IV in a VIX spike (SynthVolModel::spy, κ=1):");
    let m = SynthVolModel::spy();
    for v in [20.0, 40.0, 82.7] {
        println!("    VIX {v:>5.1}: ATM 1y {:.1}%  1.5y {:.1}%  (2020-03-16 reference: 1y SPX ATM ≈ 30–40%)", m.atm_vol(1.0, v) * 100.0, m.atm_vol(1.5, v) * 100.0);
    }
    println!("R4  requested vs realised exposure when max_premium_pct binds (flat market):");
    let flat = {
        let mut d = data.clone();
        for b in d.prices.bars.iter_mut() { b.close = 100.0; b.adj_close = 100.0; }
        for p in d.vol_index.points.iter_mut() { p.1 = 20.0; }
        d
    };
    for (dl, e) in [(0.8, 3.0), (0.9, 2.0), (0.9, 3.0)] {
        let r = run_backtest(&flat, &RollConfig { strike: StrikeSelection::Delta(dl), target_exposure: e, ..RollConfig::qqq() }).unwrap();
        println!("    Δ{dl} target {e:.1}x → realised avg exposure {:.2}x", r.avg_exposure);
    }
}
