//! `optlab` — command line front-end of OptLab.
//!
//! Runs every engine feature headless (useful for scripting, CI and verifying the
//! engine independently of the GUI). All historical commands work offline on the
//! bundled datasets; `chain` uses live providers when the network allows.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use clap::{Parser, Subcommand, ValueEnum};
use optlab_core::advisor::{advise, strategy_conclusions, MarketSnapshot};
use optlab_core::backtest::{run_backtest, BacktestData, BacktestResult, RollConfig, StrikeSelection};
use optlab_core::optimize::{bootstrap_robustness, optimize, optimize_robust, walk_forward, walk_forward_summary, Objective, ParamGrid, RobustOptions};
use optlab_core::stats::PerformanceStats;
use optlab_core::strategy::{build_from_chain, build_synthetic, MarketParams, StrategyTemplate, ALL_TEMPLATES};
use optlab_core::surface::ChainAnalytics;
use optlab_data::{bundled, presets, DataService, Provider};

#[derive(Parser)]
#[command(name = "optlab", version, about = "OptLab 選擇權實驗室 — LEAPS 滾動回測、策略分析與期權鏈數據")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Shared LEAPS roll parameters.
#[derive(clap::Args, Clone)]
struct RollArgs {
    /// Underlying with bundled history (QQQ, SPY, NVDA, MSFT, ...).
    #[arg(long, default_value = "QQQ")]
    symbol: String,
    /// Target call delta at entry.
    #[arg(long, default_value_t = 0.80)]
    delta: f64,
    /// Calendar DTE when opening/rolling into a new LEAPS.
    #[arg(long, default_value_t = 540)]
    entry_dte: u32,
    /// Roll when DTE falls to this.
    #[arg(long, default_value_t = 180)]
    roll_dte: u32,
    /// Delta-adjusted exposure / equity (1.5 = 150%).
    #[arg(long, default_value_t = 1.5)]
    exposure: f64,
    /// Rebalance band (0.25 = resize when exposure leaves ±25% of target; 0 = only at rolls).
    #[arg(long, default_value_t = 0.25)]
    band: f64,
    /// Roll up (re-strike) when delta exceeds this.
    #[arg(long)]
    roll_up: Option<f64>,
    /// Half bid/ask spread as a fraction of option price (overrides preset).
    #[arg(long)]
    half_spread: Option<f64>,
    #[arg(long, default_value_t = 100_000.0)]
    capital: f64,
    /// Whole contracts only (realistic for small accounts).
    #[arg(long)]
    integer: bool,
    /// Long-run ATM IV of the synthetic vol model (e.g. from `optlab chain --calibrate`).
    #[arg(long)]
    lr_vol: Option<f64>,
    /// Short-dated ATM IV / vol index ratio of the synthetic vol model.
    #[arg(long)]
    iv_scale: Option<f64>,
    /// Execution lag in trading days (signal at close t, fill at close t+lag).
    #[arg(long)]
    lag: Option<u32>,
    /// Use the conservative cost model (wider spreads, slippage, stronger crisis widening).
    #[arg(long)]
    conservative: bool,
    /// Start date YYYY-MM-DD.
    #[arg(long)]
    start: Option<NaiveDate>,
    /// End date YYYY-MM-DD.
    #[arg(long)]
    end: Option<NaiveDate>,
}

impl RollArgs {
    fn config(&self) -> RollConfig {
        let mut c = presets::roll_config_for(&self.symbol);
        c.strike = StrikeSelection::Delta(self.delta);
        c.entry_dte = self.entry_dte;
        c.roll_dte = self.roll_dte;
        c.target_exposure = self.exposure;
        c.rebalance_band = (self.band > 0.0).then_some(self.band);
        c.roll_up_delta = self.roll_up;
        if let Some(h) = self.half_spread {
            c.costs.half_spread_pct = h;
        }
        if let Some(v) = self.lr_vol {
            c.vol_model.long_run_vol = v;
        }
        if let Some(v) = self.iv_scale {
            c.vol_model.iv_scale = v;
        }
        if let Some(l) = self.lag {
            c.exec_lag_days = l;
        }
        if self.conservative {
            c.costs = optlab_core::synth::CostModel::conservative();
        }
        c.initial_capital = self.capital;
        c.integer_contracts = self.integer;
        c.start = self.start;
        c.end = self.end;
        c
    }
    fn data(&self) -> Result<BacktestData> {
        Ok(presets::backtest_data(&self.symbol)?.0)
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Obj {
    Cagr,
    Sharpe,
    Calmar,
    Sortino,
    Utility,
    DdCap,
}

#[derive(Subcommand)]
enum Cmd {
    /// List bundled datasets.
    Data,
    /// Run one rolling-LEAPS backtest.
    Backtest {
        #[command(flatten)]
        roll: RollArgs,
        /// Print the trade log.
        #[arg(long)]
        trades: bool,
        /// Write the full result (records, trades, stats) as JSON.
        #[arg(long)]
        json: Option<std::path::PathBuf>,
        /// Write daily equity curves (strategy + benchmarks) as CSV.
        #[arg(long)]
        csv: Option<std::path::PathBuf>,
    },
    /// Grid-search roll parameters.
    Optimize {
        #[command(flatten)]
        roll: RollArgs,
        #[arg(long, value_enum, default_value = "calmar")]
        objective: Obj,
        /// λ for the utility objective (CAGR − λ·|MaxDD|) or the drawdown cap for dd-cap.
        #[arg(long, default_value_t = 0.5)]
        param: f64,
        #[arg(long, default_value_t = 15)]
        top: usize,
        /// Use the small (36 combo) grid.
        #[arg(long)]
        small: bool,
        /// Score each combo by the median over bootstrap paths + neighbour smoothing
        /// (recommended: resists fitting a single historical path).
        #[arg(long)]
        robust: bool,
    },
    /// Block-bootstrap robustness of one configuration.
    Robust {
        #[command(flatten)]
        roll: RollArgs,
        #[arg(long, default_value_t = 300)]
        samples: usize,
        #[arg(long, default_value_t = 20.0)]
        block_days: f64,
        #[arg(long, default_value_t = 42)]
        seed: u64,
    },
    /// Walk-forward re-optimisation (train N years, test M years).
    WalkForward {
        #[command(flatten)]
        roll: RollArgs,
        #[arg(long, default_value_t = 8.0)]
        train: f64,
        #[arg(long, default_value_t = 3.0)]
        test: f64,
        #[arg(long, value_enum, default_value = "calmar")]
        objective: Obj,
    },
    /// Stress test: open fresh at the start of each historical crash window.
    Crash {
        #[command(flatten)]
        roll: RollArgs,
        /// Comma separated exposures to compare.
        #[arg(long, default_value = "1.0,1.5,2.0")]
        exposures: String,
    },
    /// Market snapshot + recommendations for a bundled symbol.
    Advise {
        #[arg(long, default_value = "QQQ")]
        symbol: String,
        /// Also print the built-in strategy conclusions knowledge base.
        #[arg(long)]
        kb: bool,
    },
    /// Fetch an option chain (live provider, snapshot or CSV) and print analytics.
    Chain {
        #[arg(long, default_value = "QQQ")]
        symbol: String,
        /// cboe | yahoo | tradier | polygon | auto | snapshot
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Import from a CSV file instead.
        #[arg(long)]
        csv: Option<std::path::PathBuf>,
        /// Load a chain JSON file (e.g. a saved snapshot or a CBOE delayed-quotes capture).
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Build this strategy template on the chain (e.g. leaps, pmcc).
        #[arg(long)]
        strategy: Option<String>,
    },
    /// List strategy templates, or analyse one on a synthetic market.
    Strategies {
        /// Template key (leaps, pmcc, bull_call_spread, ...). Omit to list.
        template: Option<String>,
        #[arg(long, default_value_t = 600.0)]
        spot: f64,
        #[arg(long, default_value_t = 0.20)]
        iv: f64,
        #[arg(long, default_value_t = 0.04)]
        rate: f64,
        #[arg(long, default_value_t = 0.006)]
        div: f64,
    },
}

fn objective(o: Obj, p: f64) -> Objective {
    match o {
        Obj::Cagr => Objective::Cagr,
        Obj::Sharpe => Objective::Sharpe,
        Obj::Calmar => Objective::Calmar,
        Obj::Sortino => Objective::Sortino,
        Obj::Utility => Objective::Utility { lambda: p },
        Obj::DdCap => Objective::MaxCagrWithDdCap { max_dd: p },
    }
}

fn pct(x: f64) -> String {
    format!("{:.1}%", x * 100.0)
}

fn stats_row(name: &str, s: &PerformanceStats) {
    println!(
        "  {:<26} CAGR {:>7}  波動 {:>6}  Sharpe {:>5.2}  最大回撤 {:>7}  Calmar {:>5.2}  最差年 {:>14}  終值 x{:.2}",
        name,
        pct(s.cagr),
        pct(s.ann_vol),
        s.sharpe,
        pct(s.max_drawdown),
        s.calmar,
        s.worst_year.map_or("-".into(), |(y, r)| format!("{y} {}", pct(r))),
        s.final_value / s.initial_value.max(1e-9),
    );
}

fn print_result(r: &BacktestResult) {
    println!(
        "{}  {} → {} ({:.1} 年)  轉倉 {} 次  交易成本 ${:.0}（{:.2}%/年）  平均曝險 {:.2}x  持有成本 {:.2}%/年（名目）  最低權益 {}{}",
        r.label(),
        r.stats.start,
        r.stats.end,
        r.stats.years,
        r.n_rolls,
        r.total_costs,
        r.cost_drag_annual * 100.0,
        r.avg_exposure,
        r.avg_carry_pct_of_exposure * 100.0,
        pct(r.min_equity_ratio),
        if r.wiped_out {
            format!("  ⚠ 爆倉 {}", r.wiped_date.map(|d| d.to_string()).unwrap_or_default())
        } else if r.near_wipe {
            "  ⚠ 近乎歸零".to_string()
        } else {
            String::new()
        }
    );
    stats_row("LEAPS 滾動", &r.stats);
    for (n, s) in &r.benchmark_stats {
        stats_row(n, s);
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Data => {
            for b in bundled::list() {
                println!("{:<8} {:?} {} → {}  {}  [{}]", b.symbol, b.kind, b.first, b.last, b.description, b.source);
            }
        }
        Cmd::Backtest { roll, trades, json, csv } => {
            let data = roll.data()?;
            let r = run_backtest(&data, &roll.config())?;
            print_result(&r);
            println!("  逐年報酬:");
            for (y, v) in &r.stats.yearly {
                let bh = r.benchmark_stats.first().and_then(|(_, s)| s.yearly.iter().find(|p| p.0 == *y)).map(|p| p.1);
                println!("    {y}  {:>8}   買進持有 {:>8}", pct(*v), bh.map_or("-".into(), pct));
            }
            if trades {
                for t in &r.trades {
                    println!(
                        "  {} {:?} {:+.2} 口 K={:.2} 到期 {} @ {:.2}  S={:.2} Δ={:.2} IV={:.1}% 成本 {:.2}  {}",
                        t.date, t.action, t.contracts, t.strike, t.expiry, t.price, t.underlying, t.delta, t.iv * 100.0, t.cost, t.reason
                    );
                }
            }
            if let Some(p) = json {
                std::fs::write(&p, serde_json::to_string_pretty(&r)?).with_context(|| format!("write {}", p.display()))?;
                println!("已寫出 {}", p.display());
            }
            if let Some(p) = csv {
                let mut out = String::from("date,leaps");
                for (n, _) in &r.benchmarks {
                    out.push(',');
                    out.push_str(n);
                }
                out.push('\n');
                for (i, rec) in r.records.iter().enumerate() {
                    out.push_str(&format!("{},{:.2}", rec.date, rec.equity));
                    for (_, c) in &r.benchmarks {
                        out.push_str(&c.get(i).filter(|x| x.0 == rec.date).map_or(",".into(), |x| format!(",{:.2}", x.1)));
                    }
                    out.push('\n');
                }
                std::fs::write(&p, out)?;
                println!("已寫出 {}", p.display());
            }
        }
        Cmd::Optimize { roll, objective: o, param, top, small, robust } => {
            let data = roll.data()?;
            let grid = if small { ParamGrid::small() } else { ParamGrid::default() };
            let base = roll.config();
            let n = grid.len(&base);
            eprintln!("搜尋 {n} 組參數…");
            let t0 = std::time::Instant::now();
            let rows = if robust {
                optimize_robust(&data, &base, &grid, objective(o, param), &RobustOptions::default(), None)?
            } else {
                optimize(&data, &base, &grid, objective(o, param), None)
            };
            eprintln!("完成，用時 {:.2}s", t0.elapsed().as_secs_f64());
            println!("{:<4} {:<46} {:>8} {:>8} {:>8} {:>7} {:>7} {:>8}", "#", "參數", "分數", "CAGR", "最大回撤", "Sharpe", "Calmar", "最低權益");
            for (i, r) in rows.iter().take(top).enumerate() {
                println!(
                    "{:<4} {:<46} {:>8.3} {:>8} {:>8} {:>7.2} {:>7.2} {:>8}",
                    i + 1,
                    r.label(),
                    r.score,
                    pct(r.stats.cagr),
                    pct(r.stats.max_drawdown),
                    r.stats.sharpe,
                    r.stats.calmar,
                    pct(r.min_equity_ratio)
                );
            }
            if robust {
                println!("\n分數 = 12 條拔靴路徑 + 真實歷史的中位數，並與相鄰格點平滑。");
            } else {
                println!("\n提醒：單一歷史路徑的網格搜尋會過度擬合，建議加上 --robust，並用 walk-forward / robust 指令檢驗。");
            }
        }
        Cmd::Robust { roll, samples, block_days, seed } => {
            let data = roll.data()?;
            let cfg = roll.config();
            let r = bootstrap_robustness(&data, &cfg, samples, block_days, seed)?;
            println!("{}  區塊拔靴法 {} 條合成歷史（區塊 {} 天，每條 {:.1} 年）", cfg.label(), r.n_samples, r.block_days, r.years);
            let row = |n: &str, p: &optlab_core::optimize::Percentiles| {
                println!("  {:<18} p5 {:>7}  p25 {:>7}  中位 {:>7}  p75 {:>7}  p95 {:>7}", n, pct(p.p5), pct(p.p25), pct(p.p50), pct(p.p75), pct(p.p95))
            };
            row("LEAPS CAGR", &r.cagr);
            row("LEAPS 最大回撤", &r.max_dd);
            row("買進持有 CAGR", &r.bh_cagr);
            row("買進持有 最大回撤", &r.bh_max_dd);
            row("槓桿ETF CAGR", &r.lev_cagr);
            row("槓桿ETF 最大回撤", &r.lev_max_dd);
            row("曝險配對 CAGR", &r.matched_cagr);
            row("最低權益/初始", &r.min_equity);
            println!(
                "  爆倉機率 {}  近乎歸零(<10%) {}  虧損機率 {}  回撤>50% 機率 {}  勝過買進持有 {}  勝過同倍槓桿ETF {}  勝過曝險配對 {}",
                pct(r.prob_wipeout),
                pct(r.prob_near_wipe),
                pct(r.prob_loss),
                pct(r.prob_dd_over_50),
                pct(r.prob_beat_benchmark),
                pct(r.prob_beat_lev_etf),
                pct(r.prob_beat_matched)
            );
        }
        Cmd::WalkForward { roll, train, test, objective: o } => {
            let data = roll.data()?;
            let w = walk_forward(&data, &roll.config(), &ParamGrid::small(), objective(o, 0.5), train, test)?;
            for x in &w {
                println!(
                    "  訓練 {}→{}  測試 {}→{}  選出 {:<30} 樣本外 CAGR {:>7} MDD {:>7} | 預設 {:>7} | 買進持有 {:>7}",
                    x.train_start,
                    x.train_end,
                    x.test_start,
                    x.test_end,
                    x.best_config.label(),
                    pct(x.oos_stats.cagr),
                    pct(x.oos_stats.max_drawdown),
                    pct(x.base_oos_stats.cagr),
                    pct(x.bh_oos_stats.cagr)
                );
            }
            let s = walk_forward_summary(&w);
            println!(
                "串接樣本外 {:.1} 年：優化 CAGR {}  預設 {}  買進持有 {}；最差回撤 優化 {} / 預設 {} / 買進持有 {}；優化勝出窗口比例 {}",
                s.years,
                pct(s.oos_cagr),
                pct(s.base_cagr),
                pct(s.bh_cagr),
                pct(s.worst_oos_dd),
                pct(s.worst_base_dd),
                pct(s.worst_bh_dd),
                pct(s.pct_windows_opt_beats_base)
            );
        }
        Cmd::Crash { roll, exposures } => {
            let data = roll.data()?;
            let exps: Vec<f64> = exposures.split(',').filter_map(|x| x.trim().parse().ok()).collect();
            if exps.is_empty() {
                bail!("--exposures must list numbers, e.g. 1.0,1.5,2.0");
            }
            let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
            let windows = [
                ("2000 網路泡沫", "2000-03-01", "2003-03-31"),
                ("2007 金融海嘯", "2007-10-01", "2009-06-30"),
                ("2011 歐債", "2011-04-01", "2012-03-31"),
                ("2015-16 人民幣", "2015-07-15", "2016-06-30"),
                ("2018 Q4", "2018-09-01", "2019-04-30"),
                ("2020 COVID", "2020-02-01", "2020-08-31"),
                ("2022 升息熊市", "2021-11-15", "2023-06-30"),
                ("2025 關稅", "2025-02-01", "2025-07-31"),
            ];
            let first = data.prices.first_date().unwrap();
            print!("{:<16}", "區間");
            print!(" {:>17}", "買進持有 報酬/回撤");
            for e in &exps {
                print!(" {:>17}", format!("{e:.2}x 報酬/回撤"));
            }
            println!();
            for (name, s, e) in windows {
                if d(s) < first {
                    continue;
                }
                print!("{name:<16}");
                let mut bh_done = false;
                for e_ in &exps {
                    let mut c = roll.config();
                    c.target_exposure = *e_;
                    c.start = Some(d(s));
                    c.end = Some(d(e));
                    let r = run_backtest(&data, &c)?;
                    if !bh_done {
                        let b = &r.benchmark_stats[0].1;
                        print!(" {:>8}/{:>8}", pct(b.total_return), pct(b.max_drawdown));
                        bh_done = true;
                    }
                    print!(" {:>8}/{:>8}{}", pct(r.stats.total_return), pct(r.stats.max_drawdown), if r.wiped_out || r.near_wipe { "⚠" } else { " " });
                }
                println!();
            }
            println!("（每個區間從起點以全新部位建倉；報酬含區間內的反彈；⚠ = 權益曾低於初始 10%）");
        }
        Cmd::Advise { symbol, kb } => {
            let prices = bundled::prices(&symbol).with_context(|| format!("no bundled data for {symbol}"))?;
            let vol = bundled::default_vol_index_for(&symbol).context("no vol index")?;
            let snap = MarketSnapshot::from_history(&prices, &vol);
            println!(
                "{} {}  價格 {:.2}  波動指數 {:.1}（1 年 IV rank {:.0}%，百分位 {:.0}%）  RV20 {:.1}%  距高點 {}  {} 200 日均線",
                snap.symbol,
                snap.date,
                snap.spot,
                snap.vol_now,
                snap.iv_rank_1y * 100.0,
                snap.iv_percentile_1y * 100.0,
                snap.rv20 * 100.0,
                pct(snap.drawdown_from_high),
                if snap.above_sma200 { "高於" } else { "低於" }
            );
            let cfg = presets::roll_config_for(&symbol);
            for a in advise(&snap, None, Some(&cfg)) {
                println!("\n[{}] {}\n{}", a.level.label(), a.title, a.body);
            }
            if kb {
                println!("\n==== 常見策略綜合結論 ====");
                for a in strategy_conclusions() {
                    println!("\n[{}] {}\n{}", a.level.label(), a.title, a.body);
                }
            }
        }
        Cmd::Chain { symbol, provider, csv, file, strategy } => {
            let svc = DataService::load_default()?;
            let chain = if let Some(p) = csv {
                svc.import_chain_csv(&p, &Default::default())?
            } else if let Some(p) = file {
                let txt = std::fs::read_to_string(&p)?;
                serde_json::from_str(&txt)
                    .or_else(|_| optlab_data::providers::cboe::parse_chain(&txt, &symbol))
                    .with_context(|| format!("{} is neither an OptLab snapshot nor a CBOE delayed-quotes JSON", p.display()))?
            } else {
                match provider.as_str() {
                    "auto" => svc.fetch_chain_auto(&symbol)?,
                    "snapshot" => svc.latest_snapshot(&symbol)?.context("no saved snapshot")?,
                    other => svc.fetch_chain(other.parse::<Provider>().map_err(|e| anyhow::anyhow!("{e}"))?, &symbol)?,
                }
            };
            let r = svc.risk_free_rate();
            let q = presets::default_dividend_yield(&chain.underlying);
            println!("{} 現價 {:.2}  {}  來源 {}  合約 {} 筆  到期日 {} 個", chain.underlying, chain.spot, chain.as_of, chain.source, chain.quotes.len(), chain.expiries().len());
            let a = ChainAnalytics::compute(&chain, r, q);
            println!("ATM IV 期限結構:");
            for (dte, iv) in a.term_structure.iter().take(14) {
                println!("  {dte:>5} 天  {:.1}%", iv * 100.0);
            }
            println!("預期波動（ATM 跨式）:");
            for m in a.expected_moves.iter().take(8) {
                println!("  {} ({:>4}d) ±{:.2} ({:.1}%)  區間 {:.2} – {:.2}", m.expiry, m.dte, m.straddle, m.move_pct * 100.0, m.lower, m.upper);
            }
            println!(
                "Put/Call 比：成交量 {}  未平倉 {}",
                a.put_call_total.volume_ratio.map_or("-".into(), |x| format!("{x:.2}")),
                a.put_call_total.oi_ratio.map_or("-".into(), |x| format!("{x:.2}"))
            );
            for mp in a.max_pain.iter().take(4) {
                println!("最大痛點（{}）: {:.2}", mp.expiry, mp.strike);
            }
            println!("GEX 總量 ${:.0}/1%  Gamma 翻轉點 {:?}", a.gex.total_gex, a.gex.gamma_flip);
            for (e, s) in a.skew_25d.iter().take(6) {
                println!("  25Δ skew {e}: {:.2} vol pts", s * 100.0);
            }
            // Calibrate the backtest's synthetic vol model to this chain's ATM term structure.
            let vname = bundled::default_vol_index_name(&chain.underlying);
            if let Ok(vs) = svc.fetch_vol_index(vname) {
                if let Some((d, v)) = vs.last() {
                    let base = presets::vol_model_for(&chain.underlying);
                    match base.calibrate_term_structure(&a.term_structure, v) {
                        Some((m, rms)) => println!(
                            "合成波動模型校準（{vname} {v:.2} @ {d}）：long_run_vol {:.3}（預設 {:.3}）  iv_scale {:.3}（預設 {:.3}）  RMS {rms:.2} vol 點\n  → 回測可用：optlab backtest --symbol {} --lr-vol {:.3} --iv-scale {:.3}",
                            m.long_run_vol, base.long_run_vol, m.iv_scale, base.iv_scale, chain.underlying, m.long_run_vol, m.iv_scale
                        ),
                        None => println!("期限結構點數不足，無法校準合成波動模型"),
                    }
                    if (d - chain.as_of.date()).num_days().abs() > 5 {
                        println!("  ⚠ 波動指數日期 {d} 與期權鏈日期 {} 不同，校準僅供參考", chain.as_of.date());
                    }
                }
            }
            if let Some(key) = strategy {
                let t = StrategyTemplate::from_key(&key).with_context(|| format!("unknown template {key}"))?;
                let mut tp = t.default_params();
                tp.r = r;
                tp.q = q;
                let s = build_from_chain(t, &chain, &tp)?;
                print_strategy(&s, &MarketParams::new(chain.spot, r, q), a.surface.iv(chain.spot, 1.0).unwrap_or(0.2));
            }
        }
        Cmd::Strategies { template, spot, iv, rate, div } => match template {
            None => {
                for t in ALL_TEMPLATES {
                    println!("{:<18} {} / {}  [{}]  槓桿做多適配度 {}/5", t.key(), t.name_zh(), t.name_en(), t.outlook().label_zh(), t.bull_leverage_fit());
                }
            }
            Some(k) => {
                let t = StrategyTemplate::from_key(&k).with_context(|| format!("unknown template {k}"))?;
                println!("{} / {}\n{}\n", t.name_zh(), t.name_en(), t.description_zh());
                let mut tp = t.default_params();
                tp.r = rate;
                tp.q = div;
                let today = chrono::Local::now().date_naive();
                let s = build_synthetic(t, spot, iv, rate, div, today, &tp);
                print_strategy(&s, &MarketParams::new(spot, rate, div), iv);
            }
        },
    }
    Ok(())
}

fn print_strategy(s: &optlab_core::strategy::Strategy, p: &MarketParams, sigma: f64) {
    println!("策略：{}", s.name);
    for l in &s.legs {
        println!("  {}", l.describe());
    }
    let date = s.expiry_date();
    let sum = s.summary(date, p, sigma, p.r - p.q);
    println!(
        "  建倉成本 ${:.0}  所需資金 ${:.0}  損益兩平 {:?}  最大獲利 {}  最大虧損 {}  勝率(風險中性) {:.1}%  期望損益 ${:.0}  槓桿 {:.2}x",
        sum.entry_cost,
        sum.capital_required,
        sum.breakevens.iter().map(|b| (b * 100.0).round() / 100.0).collect::<Vec<_>>(),
        if sum.max_profit_loss.profit_unbounded { "無上限".to_string() } else { format!("${:.0}", sum.max_profit_loss.max_profit) },
        if sum.max_profit_loss.loss_unbounded { "無下限".to_string() } else { format!("${:.0}", sum.max_profit_loss.max_loss) },
        sum.probability_of_profit * 100.0,
        sum.expected_pnl,
        sum.leverage
    );
    let g = s.greeks(p.spot, s.created, p);
    println!("  Greeks（部位）：Δ {:.1} 股  Γ {:.3}  Θ ${:.2}/天  Vega ${:.2}/vol點", g.delta, g.gamma, g.theta, g.vega);
}
