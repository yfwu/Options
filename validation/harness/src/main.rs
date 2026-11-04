//! Independent validation harness for optlab-core (NOT part of the OptLab workspace).
//!
//! Usage:
//!   optlab-validation-harness pricing <out.csv>
//!   optlab-validation-harness synth <out.csv>
//!   optlab-validation-harness jobs <jobs.json> <summary.jsonl>
//!   optlab-validation-harness boot <boot.json> <out.jsonl>
//!
//! It only uses the *public* API of optlab-core (run_backtest, RollConfig, SynthVolModel,
//! pricing::*). Data loading mirrors crates/core/examples/leaps_study.rs, plus a few
//! alternative data constructions used to probe biases (real VXN from 2000-10,
//! FRED DGS3MO rates instead of the built-in approximation).

use anyhow::{anyhow, Context, Result};
use chrono::{Datelike, Duration, NaiveDate};
use optlab_core::backtest::{approx_us_tbill_rates, run_backtest, BacktestData, BacktestResult, RateSource, RollConfig};
use optlab_core::chain::OptionKind;
use optlab_core::history::{Bar, PriceSeries, ScalarSeries};
use optlab_core::pricing::{bs_greeks, bs_price, implied_vol, strike_for_delta};
use optlab_core::synth::{CostModel, SynthVolModel};
use rand::{rngs::StdRng, Rng, SeedableRng};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data")
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().or_else(|| NaiveDate::parse_from_str(s, "%m/%d/%Y").ok())
}

fn read_col(file: &str, col: usize) -> Vec<(NaiveDate, f64)> {
    let txt = std::fs::read_to_string(data_dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    let mut v: Vec<(NaiveDate, f64)> = txt
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((parse_date(f.first()?)?, f.get(col)?.trim().parse::<f64>().ok()?))
        })
        .filter(|p| p.1.is_finite() && p.1 > 0.0)
        .collect();
    v.sort_by_key(|p| p.0);
    v.dedup_by_key(|p| p.0);
    v
}

/// like read_col but keeps zero values (rates)
fn read_col_nonneg(file: &str, col: usize) -> Vec<(NaiveDate, f64)> {
    let txt = std::fs::read_to_string(data_dir().join(file)).unwrap();
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Some((parse_date(f.first()?)?, f.get(col)?.trim().parse::<f64>().ok()?))
        })
        .filter(|p| p.1.is_finite() && p.1 >= 0.0)
        .collect()
}

fn series_from(name: &str, pts: &[(NaiveDate, f64)]) -> PriceSeries {
    let bars = pts.iter().map(|&(d, p)| Bar { date: d, open: p, high: p, low: p, close: p, adj_close: p, volume: 0.0 }).collect();
    PriceSeries::new(name, bars)
}

/// QQQ vol index exactly as leaps_study.rs: max(VIX·ratio, 1.1·RV21) before CBOE VXN (2009-09),
/// optionally replacing 2000-10..2009-09 by the Barchart VXN history.
fn qqq_vol(qqq: &[(NaiveDate, f64)], use_barchart: bool) -> ScalarSeries {
    let vxn = ScalarSeries::new("VXN", read_col("VXN_History.csv", 4));
    let vix = ScalarSeries::new("VIX", read_col("VIX_History.csv", 4));
    let mut ratios: Vec<f64> = vxn.points.iter().filter_map(|&(d, v)| vix.value_at(d).map(|x| v / x)).collect();
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let ratio = ratios[ratios.len() / 2];
    let first_vxn = vxn.points[0].0;
    let bc = if use_barchart { read_col("VXN_barchart_2000.csv", 4) } else { vec![] };
    let bc_first = bc.first().map(|p| p.0).unwrap_or(first_vxn);
    let lr: Vec<f64> = qqq.windows(2).map(|w| (w[1].1 / w[0].1).ln()).collect();
    let mut pts = Vec::new();
    for (i, &(d, _)) in qqq.iter().enumerate() {
        if d >= first_vxn || (use_barchart && d >= bc_first) {
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
    if use_barchart {
        pts.extend(bc.iter().copied().filter(|p| p.0 < first_vxn));
    }
    pts.extend(vxn.points.iter().copied());
    ScalarSeries::new("QQQ vol", pts)
}

fn rates(kind: &str) -> RateSource {
    match kind {
        "dgs3mo" => RateSource::Series(ScalarSeries::new("DGS3MO", read_col_nonneg("DGS3MO.csv", 1))),
        _ => RateSource::Series(approx_us_tbill_rates()),
    }
}

/// Data sets: "qqq" (developer construction), "qqq_bc" (Barchart VXN from 2000-10),
/// "spy" (VIX_SPY_1993). Suffix ":dgs3mo" switches to FRED rates. Suffix ":q=0.004" overrides q.
fn load_data(key: &str) -> Result<BacktestData> {
    let mut parts = key.split(':');
    let base = parts.next().unwrap();
    let mut rate_kind = "approx";
    let mut q_override = None;
    for p in parts {
        if p == "dgs3mo" {
            rate_kind = "dgs3mo";
        } else if let Some(q) = p.strip_prefix("q=") {
            q_override = Some(q.parse::<f64>()?);
        }
    }
    let d = match base {
        "qqq" | "qqq_bc" => {
            let qqq = read_col("QQQ_adj_1999.csv", 1);
            let vol = qqq_vol(&qqq, base == "qqq_bc");
            BacktestData::new(series_from("QQQ", &qqq), vol, rates(rate_kind), q_override.unwrap_or(0.007))
        }
        "qqq_preset" => {
            // optlab-data preset style: VIX×2.13 before 2000-10-10, Barchart VXN to 2009-09, CBOE VXN after
            let qqq = read_col("QQQ_adj_1999.csv", 1);
            let vix = ScalarSeries::new("VIX", read_col("VIX_History.csv", 4));
            let bc = read_col("VXN_barchart_2000.csv", 4);
            let cb = read_col("VXN_History.csv", 4);
            let mut pts: Vec<(NaiveDate, f64)> = qqq.iter().filter(|p| p.0 < bc[0].0).map(|p| (p.0, vix.value_at(p.0).unwrap() * 2.13)).collect();
            pts.extend(bc.iter().copied().filter(|p| p.0 < cb[0].0));
            pts.extend(cb.iter().copied());
            BacktestData::new(series_from("QQQ", &qqq), ScalarSeries::new("VXN preset", pts), rates(rate_kind), q_override.unwrap_or(0.007))
        }
        "spy" => {
            let spy = read_col("VIX_SPY_1993.csv", 2);
            let vix = ScalarSeries::new("VIX", read_col("VIX_SPY_1993.csv", 1));
            BacktestData::new(series_from("SPY", &spy), vix, rates(rate_kind), q_override.unwrap_or(0.017))
        }
        _ => return Err(anyhow!("unknown data {key}")),
    };
    Ok(d)
}

fn merge(a: &mut Value, b: &Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                merge(a.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b.clone(),
    }
}

fn base_cfg(data_key: &str) -> RollConfig {
    if data_key.starts_with("spy") {
        RollConfig::spy()
    } else {
        RollConfig::qqq()
    }
}

fn make_cfg(data_key: &str, overrides: &Value) -> Result<RollConfig> {
    let mut v = serde_json::to_value(base_cfg(data_key))?;
    merge(&mut v, overrides);
    Ok(serde_json::from_value(v)?)
}

#[derive(Deserialize)]
struct Job {
    name: String,
    data: String,
    #[serde(default)]
    cfg: Value,
    /// write daily records CSV here
    #[serde(default)]
    records: Option<String>,
    /// crash-horizon (calendar days) for the *_h statistics
    #[serde(default)]
    horizon_days: Option<i64>,
}

#[derive(Serialize, Default)]
struct HStats {
    mdd: f64,
    min_eq: f64,
    end_mult: f64,
    /// calendar days from start until equity regains the initial value after the horizon minimum (None = never)
    days_to_recover_initial: Option<i64>,
}

fn h_stats(c: &[(NaiveDate, f64)], horizon: i64) -> HStats {
    let v0 = c[0].1;
    let lim = c[0].0 + Duration::days(horizon);
    let m = c.iter().take_while(|p| p.0 <= lim).count().max(2);
    let h = &c[..m];
    let mut peak = v0;
    let mut mdd: f64 = 0.0;
    let mut min_i = 0;
    for (i, &(_, v)) in h.iter().enumerate() {
        peak = peak.max(v);
        if peak > 0.0 {
            mdd = mdd.min(v / peak - 1.0);
        }
        if v < h[min_i].1 {
            min_i = i;
        }
    }
    let rec = if h[min_i].1 >= v0 { Some(0) } else { c[min_i..].iter().find(|p| p.1 >= v0).map(|p| (p.0 - c[0].0).num_days()) };
    HStats { mdd, min_eq: h[min_i].1 / v0, end_mult: h[m - 1].1 / v0, days_to_recover_initial: rec }
}

#[derive(Serialize, Default)]
struct CurveStats {
    final_mult: f64,
    cagr: f64,
    mdd: f64,
    mdd_trough: String,
    min_eq: f64,
    /// calendar days from start until the curve first regains the initial value after its minimum
    days_to_recover_initial: Option<i64>,
    /// calendar days from the max-drawdown trough to regaining the prior peak
    days_trough_to_recover_peak: Option<i64>,
}

fn curve_stats(c: &[(NaiveDate, f64)]) -> CurveStats {
    let v0 = c[0].1;
    let mut peak = v0;
    let mut mdd = 0.0;
    let mut trough_i = 0;
    let mut peak_at_mdd = v0;
    let mut min_i = 0;
    for (i, &(_, v)) in c.iter().enumerate() {
        if v > peak {
            peak = v;
        }
        let dd = if peak > 0.0 { v / peak - 1.0 } else { 0.0 };
        if dd < mdd {
            mdd = dd;
            trough_i = i;
            peak_at_mdd = peak;
        }
        if v < c[min_i].1 {
            min_i = i;
        }
    }
    let yrs = (c.last().unwrap().0 - c[0].0).num_days() as f64 / 365.25;
    let fm = c.last().unwrap().1 / v0;
    let rec_init = if c[min_i].1 >= v0 { Some(0) } else { c[min_i..].iter().find(|p| p.1 >= v0).map(|p| (p.0 - c[0].0).num_days()) };
    let rec_peak = if mdd < 0.0 { c[trough_i..].iter().find(|p| p.1 >= peak_at_mdd).map(|p| (p.0 - c[trough_i].0).num_days()) } else { Some(0) };
    CurveStats {
        final_mult: fm,
        cagr: if fm <= 0.0 { -1.0 } else { fm.powf(1.0 / yrs.max(1e-9)) - 1.0 },
        mdd,
        mdd_trough: c[trough_i].0.to_string(),
        min_eq: c[min_i].1 / v0,
        days_to_recover_initial: rec_init,
        days_trough_to_recover_peak: rec_peak,
    }
}

/// Daily-rebalanced L× on the total-return series (same formula as the engine's benchmark).
fn letf(dates: &[NaiveDate], adj: &[f64], rate: &[f64], lev: f64, spread: f64, er: f64) -> Vec<(NaiveDate, f64)> {
    let mut v = 1.0;
    let mut out = vec![(dates[0], v)];
    for i in 1..dates.len() {
        let dt = (dates[i] - dates[i - 1]).num_days() as f64 / 365.0;
        let r = adj[i] / adj[i - 1] - 1.0;
        let fin = (lev - 1.0) * (rate[i - 1] + if lev > 1.0 { spread } else { 0.0 });
        v *= 1.0 + lev * r - fin * dt - er * dt;
        v = v.max(0.0);
        out.push((dates[i], v));
    }
    out
}

#[derive(Serialize)]
struct JobOut {
    name: String,
    data: String,
    label: String,
    start: String,
    end: String,
    years: f64,
    strat: CurveStats,
    bh: CurveStats,
    letf2: CurveStats,
    letf3: CurveStats,
    wiped: bool,
    wiped_date: Option<String>,
    n_rolls: usize,
    total_costs: f64,
    cost_drag_annual: f64,
    avg_exposure: f64,
    avg_extrinsic_pct_annual: f64,
    avg_carry_pct_of_exposure: f64,
    avg_theta_pct_equity: f64,
    avg_rate: f64,
    sharpe: f64,
    ann_vol: f64,
    bh_sharpe: f64,
    bh_vol: f64,
    h: Option<[HStats; 4]>,
}

fn write_records(path: &str, r: &BacktestResult) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(f, "date,equity,spot,contracts,strike,dte,delta,exposure,option_value,cash,iv,theta,bh")?;
    let bh: HashMap<NaiveDate, f64> = r.benchmarks[0].1.iter().copied().collect();
    for x in &r.records {
        writeln!(
            f,
            "{},{:.6},{:.6},{:.8},{:.6},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
            x.date, x.equity, x.spot, x.contracts, x.strike, x.dte, x.delta, x.exposure, x.option_value, x.cash, x.iv, x.theta,
            bh.get(&x.date).copied().unwrap_or(f64::NAN)
        )?;
    }
    let tp = path.replace(".csv", "_trades.csv");
    let mut f = std::io::BufWriter::new(std::fs::File::create(tp)?);
    writeln!(f, "date,action,strike,expiry,contracts,price,underlying,delta,iv,cost")?;
    for t in &r.trades {
        writeln!(f, "{},{:?},{:.6},{},{:.8},{:.6},{:.6},{:.6},{:.6},{:.6}", t.date, t.action, t.strike, t.expiry, t.contracts, t.price, t.underlying, t.delta, t.iv, t.cost)?;
    }
    Ok(())
}

fn run_jobs(jobs_path: &str, out_path: &str) -> Result<()> {
    let jobs: Vec<Job> = serde_json::from_str(&std::fs::read_to_string(jobs_path)?)?;
    let mut keys: Vec<String> = jobs.iter().map(|j| j.data.clone()).collect();
    keys.sort();
    keys.dedup();
    let datas: HashMap<String, BacktestData> = keys.iter().map(|k| (k.clone(), load_data(k).unwrap())).collect();
    let outs: Vec<Result<JobOut>> = jobs
        .par_iter()
        .map(|j| {
            let data = &datas[&j.data];
            let cfg = make_cfg(&j.data, &j.cfg).with_context(|| j.name.clone())?;
            let r = run_backtest(data, &cfg).with_context(|| j.name.clone())?;
            if let Some(p) = &j.records {
                write_records(p, &r)?;
            }
            let curve = r.equity_curve();
            let bh = &r.benchmarks[0].1;
            // ETF proxies on the same dates (rates from the engine records are not exposed, rebuild)
            let dates: Vec<NaiveDate> = curve.iter().map(|p| p.0).collect();
            let adj: Vec<f64> = bh.iter().map(|p| p.1).collect();
            let rate: Vec<f64> = dates.iter().map(|&d| data.rate.rate_at(d)).collect();
            let l2 = letf(&dates, &adj, &rate, 2.0, 0.005, 0.0095);
            let l3 = letf(&dates, &adj, &rate, 3.0, 0.005, 0.0095);
            Ok(JobOut {
                name: j.name.clone(),
                data: j.data.clone(),
                label: cfg.label(),
                start: r.stats.start.to_string(),
                end: r.stats.end.to_string(),
                years: r.stats.years,
                strat: curve_stats(&curve),
                bh: curve_stats(bh),
                letf2: curve_stats(&l2),
                letf3: curve_stats(&l3),
                wiped: r.wiped_out,
                wiped_date: r.wiped_date.map(|d| d.to_string()),
                n_rolls: r.n_rolls,
                total_costs: r.total_costs,
                cost_drag_annual: r.cost_drag_annual,
                avg_exposure: r.avg_exposure,
                avg_extrinsic_pct_annual: r.avg_extrinsic_pct_annual,
                avg_carry_pct_of_exposure: r.avg_carry_pct_of_exposure,
                avg_theta_pct_equity: r.avg_theta_pct_equity,
                avg_rate: r.avg_rate,
                sharpe: r.stats.sharpe,
                ann_vol: r.stats.ann_vol,
                bh_sharpe: r.benchmark_stats[0].1.sharpe,
                bh_vol: r.benchmark_stats[0].1.ann_vol,
                h: j.horizon_days.map(|hd| [h_stats(&curve, hd), h_stats(bh, hd), h_stats(&l2, hd), h_stats(&l3, hd)]),
            })
        })
        .collect();
    let mut f = std::io::BufWriter::new(std::fs::File::create(out_path)?);
    let mut nerr = 0;
    for o in outs {
        match o {
            Ok(o) => writeln!(f, "{}", serde_json::to_string(&o)?)?,
            Err(e) => {
                nerr += 1;
                eprintln!("job error: {e:#}");
            }
        }
    }
    eprintln!("{} jobs, {} errors", jobs.len(), nerr);
    Ok(())
}

// ---------------------------------------------------------------------------
// Pricing / synth grids
// ---------------------------------------------------------------------------

fn pricing_grid(out: &str) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(out)?);
    writeln!(f, "kind,s,k,t,r,q,sigma,price,delta,gamma,theta,vega,rho,iv,k_for_delta80")?;
    for kind in [OptionKind::Call, OptionKind::Put] {
        for &s in &[100.0] {
            for &k in &[50.0, 70.0, 80.0, 90.0, 95.0, 100.0, 105.0, 110.0, 130.0, 160.0] {
                for &t in &[0.02, 0.1, 0.25, 0.5, 1.0, 1.5, 2.0, 3.0] {
                    for &r in &[0.0, 0.02, 0.05, 0.10] {
                        for &q in &[0.0, 0.007, 0.02] {
                            for &sig in &[0.08, 0.15, 0.25, 0.40, 0.80, 1.5] {
                                let g = bs_greeks(kind, s, k, t, r, q, sig);
                                let p = bs_price(kind, s, k, t, r, q, sig);
                                let iv = implied_vol(kind, p, s, k, t, r, q).unwrap_or(f64::NAN);
                                let kd = if kind == OptionKind::Call { strike_for_delta(kind, 0.8, s, t, r, q, sig) } else { strike_for_delta(kind, -0.25, s, t, r, q, sig) };
                                writeln!(
                                    f,
                                    "{:?},{s},{k},{t},{r},{q},{sig},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e}",
                                    kind, p, g.delta, g.gamma, g.theta, g.vega, g.rho, iv, kd
                                )?;
                            }
                        }
                    }
                }
            }
        }
    }
    // Hull textbook points
    for (kind, s, k, t, r, q, sig) in [
        (OptionKind::Call, 42.0, 40.0, 0.5, 0.10, 0.0, 0.20),
        (OptionKind::Put, 42.0, 40.0, 0.5, 0.10, 0.0, 0.20),
        (OptionKind::Call, 49.0, 50.0, 20.0 / 52.0, 0.05, 0.0, 0.20),
        (OptionKind::Put, 49.0, 50.0, 20.0 / 52.0, 0.05, 0.0, 0.20),
    ] {
        let g = bs_greeks(kind, s, k, t, r, q, sig);
        let iv = implied_vol(kind, g.price, s, k, t, r, q).unwrap_or(f64::NAN);
        writeln!(f, "{:?},{s},{k},{t},{r},{q},{sig},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e},{:.15e},nan", kind, g.price, g.delta, g.gamma, g.theta, g.vega, g.rho, iv)?;
    }
    Ok(())
}

fn synth_grid(out: &str) -> Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(out)?);
    writeln!(f, "model,vix,t,k_over_s,atm,iv,kd80,price_kd80,delta_kd80")?;
    for (name, m, q) in [("spy", SynthVolModel::spy(), 0.017), ("qqq", SynthVolModel::qqq(), 0.007)] {
        for &vix in &[9.0, 12.0, 17.0, 25.0, 40.0, 60.0, 82.0] {
            for &t in &[0.02, 0.083, 0.25, 0.5, 1.0, 1.5, 2.0] {
                let kd = m.strike_for_delta(OptionKind::Call, 0.8, 100.0, t, 0.03, q, vix);
                let (g, _) = m.greeks(OptionKind::Call, 100.0, kd, t, 0.03, q, vix);
                for &ks in &[0.5, 0.7, 0.8, 0.9, 1.0, 1.1, 1.3] {
                    writeln!(f, "{name},{vix},{t},{ks},{:.12},{:.12},{:.12},{:.12},{:.12}", m.atm_vol(t, vix), m.iv(100.0 * ks, 100.0, t, vix), kd, g.price, g.delta)?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Block bootstrap of short crash-like horizons
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BootSpec {
    data: String,
    n: usize,
    /// trading days per synthetic path
    days: usize,
    block: f64,
    seed: u64,
    /// fraction of new-block draws forced to start on a "stress" day (vol index ≥ stress_vol)
    #[serde(default)]
    stress_frac: f64,
    #[serde(default = "default_stress_vol")]
    stress_vol: f64,
    /// start every path inside a stress regime (first block from a stress day)
    #[serde(default)]
    start_in_stress: bool,
    /// constant rate for synthetic paths
    #[serde(default = "default_rate")]
    rate: f64,
    configs: Vec<(String, Value)>,
}
fn default_stress_vol() -> f64 {
    35.0
}
fn default_rate() -> f64 {
    0.03
}

#[derive(Serialize)]
struct BootRow {
    sample: usize,
    bh: CurveStats,
    letf2: CurveStats,
    letf3: CurveStats,
    strats: Vec<(String, CurveStats, bool)>,
}

fn boot(spec_path: &str, out: &str) -> Result<()> {
    let spec: BootSpec = serde_json::from_str(&std::fs::read_to_string(spec_path)?)?;
    let data = load_data(&spec.data)?;
    // aligned source arrays
    let dates: Vec<NaiveDate> = data.prices.bars.iter().map(|b| b.date).collect();
    let adj: Vec<f64> = data.prices.bars.iter().map(|b| b.adj_close).collect();
    let vol: Vec<f64> = dates.iter().map(|&d| data.vol_index.value_at(d).unwrap().max(1.0)).collect();
    let n_src = dates.len() - 1;
    let stress: Vec<usize> = (0..n_src).filter(|&j| vol[j] >= spec.stress_vol).collect();
    let cfgs: Vec<(String, RollConfig)> = spec.configs.iter().map(|(n, v)| (n.clone(), make_cfg(&spec.data, v).unwrap())).collect();
    // synthetic calendar: business days from 2030-01-01
    let mut sd = Vec::with_capacity(spec.days + 1);
    let mut d = NaiveDate::from_ymd_opt(2030, 1, 1).unwrap();
    while sd.len() < spec.days + 1 {
        if d.weekday().num_days_from_monday() < 5 {
            sd.push(d);
        }
        d += Duration::days(1);
    }
    let rows: Vec<BootRow> = (0..spec.n)
        .into_par_iter()
        .map(|k| {
            let mut rng = StdRng::seed_from_u64(spec.seed ^ (k as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let p_new = 1.0 / spec.block.max(1.0);
            let draw = |rng: &mut StdRng, force_stress: bool| -> usize {
                if !stress.is_empty() && (force_stress || rng.gen::<f64>() < spec.stress_frac) {
                    stress[rng.gen_range(0..stress.len())]
                } else {
                    rng.gen_range(0..n_src)
                }
            };
            let mut idx = draw(&mut rng, spec.start_in_stress);
            let mut a = 100.0;
            let mut lv = vol[idx].ln();
            let mut pa = vec![a];
            let mut pv = vec![vol[idx]];
            for t in 0..spec.days {
                if t > 0 {
                    idx = if rng.gen::<f64>() < p_new { draw(&mut rng, false) } else { (idx + 1) % n_src };
                }
                a *= adj[idx + 1] / adj[idx];
                let next = lv + (vol[idx + 1] / vol[idx]).ln();
                lv = next + 0.05 * (vol[idx + 1].ln() - next);
                lv = lv.clamp(4f64.ln(), 150f64.ln());
                pa.push(a);
                pv.push(lv.exp());
            }
            let pts: Vec<(NaiveDate, f64)> = sd.iter().copied().zip(pa.iter().copied()).collect();
            let bd = BacktestData::new(
                series_from(&data.prices.symbol, &pts),
                ScalarSeries::new("vol", sd.iter().copied().zip(pv.iter().copied()).collect()),
                RateSource::Constant(spec.rate),
                data.dividend_yield,
            );
            let rate = vec![spec.rate; sd.len()];
            let bh: Vec<(NaiveDate, f64)> = pts.clone();
            let strats = cfgs
                .iter()
                .map(|(n, c)| {
                    let r = run_backtest(&bd, c).unwrap();
                    (n.clone(), curve_stats(&r.equity_curve()), r.wiped_out)
                })
                .collect();
            BootRow {
                sample: k,
                bh: curve_stats(&bh),
                letf2: curve_stats(&letf(&sd, &pa, &rate, 2.0, 0.005, 0.0095)),
                letf3: curve_stats(&letf(&sd, &pa, &rate, 3.0, 0.005, 0.0095)),
                strats,
            }
        })
        .collect();
    let mut f = std::io::BufWriter::new(std::fs::File::create(out)?);
    for r in rows {
        writeln!(f, "{}", serde_json::to_string(&r)?)?;
    }
    Ok(())
}

fn dump_inputs(out: &str) -> Result<()> {
    // aligned daily inputs the engine sees (for the Python re-implementation)
    for key in ["qqq", "qqq_bc", "spy"] {
        let d = load_data(key)?;
        let mut f = std::io::BufWriter::new(std::fs::File::create(format!("{out}/inputs_{key}.csv"))?);
        writeln!(f, "date,adj,vol,rate")?;
        for b in &d.prices.bars {
            writeln!(f, "{},{:.10},{:.6},{:.8}", b.date, b.adj_close, d.vol_index.value_at(b.date).unwrap().max(1.0), d.rate.rate_at(b.date))?;
        }
    }
    let _ = CostModel::zero();
    Ok(())
}

fn main() -> Result<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(|s| s.as_str()) {
        Some("pricing") => pricing_grid(&a[2]),
        Some("synth") => synth_grid(&a[2]),
        Some("jobs") => run_jobs(&a[2], &a[3]),
        Some("boot") => boot(&a[2], &a[3]),
        Some("inputs") => dump_inputs(&a[2]),
        Some("defaults") => {
            println!("{}", serde_json::to_string_pretty(&RollConfig::qqq())?);
            Ok(())
        }
        _ => Err(anyhow!("usage: pricing|synth|jobs|boot|inputs|defaults ...")),
    }
}
