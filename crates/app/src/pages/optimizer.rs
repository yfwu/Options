//! 參數優化 Optimizer: grid search with live progress (background, rayon inside core),
//! top-N table (click → backtest page), delta × DTE heatmap, bootstrap robustness and
//! walk-forward validation.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Subscription, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::InputState,
    progress::Progress,
    select::{Select, SelectState},
    v_flex, IndexPath, Sizable,
};
use optlab_core::backtest::{RollConfig, StrikeSelection};
use optlab_core::optimize::{bootstrap_robustness, optimize, optimize_robust, RobustOptions, walk_forward, walk_forward_summary, Objective, OptRow, ParamGrid, Percentiles, RobustnessReport, WalkForwardSummary, WalkForwardWindow};

use crate::data_bridge;
use crate::pages::backtest::BacktestParamsPatch;
use crate::state::{AppEvent, AppState, StatusLevel};
use crate::theme::palette;
use crate::widgets::charts::{fmt_pct, heatmap, xy_chart, ChartSpec, HeatmapSpec, RefLine, Series};
use crate::widgets::ui::*;

#[derive(Clone, Copy, PartialEq)]
enum Obj {
    Cagr,
    Sharpe,
    Calmar,
    Sortino,
    Utility,
    DdCap,
}

impl Obj {
    const ALL: [Obj; 6] = [Obj::Utility, Obj::Cagr, Obj::Sharpe, Obj::Sortino, Obj::Calmar, Obj::DdCap];
    fn label(self) -> &'static str {
        match self {
            Obj::Cagr => "CAGR",
            Obj::Sharpe => "Sharpe",
            Obj::Calmar => "Calmar",
            Obj::Sortino => "Sortino",
            Obj::Utility => "CAGR−λ·|DD|",
            Obj::DdCap => "DD 上限內最大 CAGR",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum HeatMetric {
    Score,
    Cagr,
    MaxDd,
    Sharpe,
}

struct Inputs {
    start: Entity<InputState>,
    end: Entity<InputState>,
    deltas: Entity<InputState>,
    entry: Entity<InputState>,
    roll: Entity<InputState>,
    exposure: Entity<InputState>,
    roll_up: Entity<InputState>,
    min_hold: Entity<InputState>,
    lambda: Entity<InputState>,
    dd_cap: Entity<InputState>,
    top_n: Entity<InputState>,
    boot_n: Entity<InputState>,
    boot_block: Entity<InputState>,
    wf_train: Entity<InputState>,
    wf_test: Entity<InputState>,
}

pub struct OptimizerPage {
    state: Entity<AppState>,
    symbol: Entity<SelectState<Vec<SharedString>>>,
    inp: Inputs,
    obj: Obj,
    robust: bool,
    metric: HeatMetric,
    errors: FormErrors,
    running: bool,
    progress: (usize, usize),
    cancel: Arc<AtomicBool>,
    rows: Vec<OptRow>,
    run_symbol: String,
    selected: Option<usize>,
    heat: Option<Rc<HeatmapSpec>>,
    boot_running: bool,
    boot: Option<(RobustnessReport, Rc<ChartSpec>, String)>,
    wf_running: bool,
    wf: Option<(Vec<WalkForwardWindow>, WalkForwardSummary)>,
    _subs: Vec<Subscription>,
}

fn parse_opt_list(t: &str) -> Result<Vec<Option<f64>>, String> {
    let mut out = vec![];
    for part in t.split([',', ' ', '，']).filter(|s| !s.trim().is_empty()) {
        let p = part.trim();
        if matches!(p.to_ascii_lowercase().as_str(), "無" | "none" | "-" | "x" | "off") {
            out.push(None);
        } else {
            out.push(Some(parse_num(p)?));
        }
    }
    Ok(out)
}

impl OptimizerPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let syms: Vec<SharedString> = data_bridge::bundled_symbols().into_iter().map(SharedString::from).collect();
        let default = state.read(cx).settings.default_symbol.to_ascii_uppercase();
        let idx = syms.iter().position(|s| s.as_ref() == default).unwrap_or(0);
        let symbol = cx.new(|cx| SelectState::new(syms, Some(IndexPath::default().row(idx)), window, cx));
        let g = ParamGrid::default();
        let join = |v: Vec<String>| v.join(", ");
        let inp = Inputs {
            start: new_input_ph("2010-01-01", "YYYY-MM-DD", window, cx),
            end: new_input_ph("", "空白 = 最新", window, cx),
            deltas: new_input(join(g.deltas.iter().map(|d| format!("{d}")).collect()), window, cx),
            entry: new_input(join(g.entry_dtes.iter().map(|d| d.to_string()).collect()), window, cx),
            roll: new_input(join(g.roll_dtes.iter().map(|d| d.to_string()).collect()), window, cx),
            exposure: new_input(join(g.exposures.iter().map(|d| format!("{d}")).collect()), window, cx),
            roll_up: new_input("無, 0.95", window, cx),
            min_hold: new_input(g.min_hold_days.to_string(), window, cx),
            lambda: new_input("0.25", window, cx),
            dd_cap: new_input("50", window, cx),
            top_n: new_input("30", window, cx),
            boot_n: new_input("200", window, cx),
            boot_block: new_input("60", window, cx),
            wf_train: new_input("6", window, cx),
            wf_test: new_input("2", window, cx),
        };
        Self {
            state,
            symbol,
            inp,
            obj: Obj::Utility,
            robust: true,
            metric: HeatMetric::Score,
            errors: FormErrors::default(),
            running: false,
            progress: (0, 0),
            cancel: Arc::new(AtomicBool::new(false)),
            rows: vec![],
            run_symbol: String::new(),
            selected: None,
            heat: None,
            boot_running: false,
            boot: None,
            wf_running: false,
            wf: None,
            _subs: vec![],
        }
    }

    fn symbol_str(&self, cx: &Context<Self>) -> String {
        self.symbol.read(cx).selected_value().map(|s| s.to_string()).unwrap_or_else(|| "QQQ".into())
    }

    fn objective(&self, e: &mut FormErrors, cx: &Context<Self>) -> Objective {
        match self.obj {
            Obj::Cagr => Objective::Cagr,
            Obj::Sharpe => Objective::Sharpe,
            Obj::Calmar => Objective::Calmar,
            Obj::Sortino => Objective::Sortino,
            Obj::Utility => Objective::Utility { lambda: e.num_range("lambda", &self.inp.lambda, 0.0, 10.0, cx) },
            Obj::DdCap => Objective::MaxCagrWithDdCap { max_dd: e.num_range("dd_cap", &self.inp.dd_cap, 1.0, 100.0, cx) / 100.0 },
        }
    }

    fn base_config(sym: &str) -> RollConfig {
        data_bridge::roll_config_for(sym)
    }

    fn build(&mut self, cx: &mut Context<Self>) -> Option<(String, RollConfig, ParamGrid, Objective)> {
        let sym = self.symbol_str(cx);
        let mut e = FormErrors::default();
        let mut base = Self::base_config(&sym);
        base.start = e.date("start", &self.inp.start, cx);
        base.end = e.date("end", &self.inp.end, cx);
        let deltas = e.list("deltas", &self.inp.deltas, cx);
        if deltas.iter().any(|d| !(0.05..=0.99).contains(d)) {
            e.errors.push(("deltas", "Delta 須介於 0.05–0.99".into()));
        }
        let entry: Vec<u32> = e.list("entry", &self.inp.entry, cx).into_iter().map(|v| v.round().max(1.0) as u32).collect();
        let roll: Vec<u32> = e.list("roll", &self.inp.roll, cx).into_iter().map(|v| v.round().max(0.0) as u32).collect();
        let exposures = e.list("exposure", &self.inp.exposure, cx);
        let roll_up = match parse_opt_list(&input_text(&self.inp.roll_up, cx)) {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => vec![None],
            Err(err) => {
                e.errors.push(("roll_up", err));
                vec![None]
            }
        };
        let min_hold = e.int("min_hold", &self.inp.min_hold, 0, 1000, cx) as u32;
        let obj = self.objective(&mut e, cx);
        let grid = ParamGrid { deltas, entry_dtes: entry, roll_dtes: roll, exposures, roll_up_deltas: roll_up, rebalance_bands: vec![], min_hold_days: min_hold };
        if e.is_empty() {
            let n = grid.len(&base);
            if n == 0 {
                e.errors.push(("entry", "沒有有效組合 (進場 DTE 須大於 滾動 DTE + 最短持有)".into()));
            } else if n > 20000 {
                e.errors.push(("deltas", format!("組合數 {n} 過多 (上限 20000)")));
            }
        }
        let ok = e.is_empty();
        self.errors = e;
        ok.then_some((sym, base, grid, obj))
    }

    pub fn run(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            self.cancel.store(true, Ordering::Relaxed);
            return;
        }
        let Some((sym, base, grid, obj)) = self.build(cx) else {
            let msg = format!("參數錯誤：{}", self.errors.summary());
            self.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, msg, cx));
            cx.notify();
            return;
        };
        let total = grid.len(&base);
        self.running = true;
        self.progress = (0, total);
        self.cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let q = data_bridge::default_dividend_yield(&sym);
        let task = self.state.update(cx, |st, cx| {
            st.set_status(StatusLevel::Info, format!("優化 {sym}：{total} 組參數…"), cx);
            st.begin_task(format!("優化 {sym}"), cx)
        });
        let total_seen = Arc::new(AtomicUsize::new(0));
        let (d2, f2_, sym2, cancel, n2) = (done.clone(), finished.clone(), sym.clone(), self.cancel.clone(), total_seen.clone());
        let robust = self.robust;
        let t0 = std::time::Instant::now();
        let job = cx.background_executor().spawn(async move {
            let res = (|| {
                let (data, _) = data_bridge::backtest_data(&sym2, q)?;
                let cb = move |k: usize, n: usize| {
                    d2.store(k, Ordering::Relaxed);
                    n2.store(n, Ordering::Relaxed);
                };
                if cancel.load(Ordering::Relaxed) {
                    return Err("已取消".to_string());
                }
                if robust {
                    optimize_robust(&data, &base, &grid, obj, &RobustOptions::default(), Some(&cb)).map_err(|e| format!("{e:#}"))
                } else {
                    Ok(optimize(&data, &base, &grid, obj, Some(&cb)))
                }
            })();
            f2_.store(true, Ordering::Relaxed);
            res
        });
        // progress poller
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let k = done.load(Ordering::Relaxed);
                let n = total_seen.load(Ordering::Relaxed);
                let fin = finished.load(Ordering::Relaxed);
                if this
                    .update(cx, |this, cx| {
                        this.progress.0 = k;
                        if n > 0 {
                            this.progress.1 = n;
                        }
                        cx.notify();
                    })
                    .is_err()
                    || fin
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this.state.update(cx, |st, cx| st.end_task(task, cx));
                match res {
                    Ok(rows) if !rows.is_empty() => {
                        let best = &rows[0];
                        let msg = format!(
                            "優化完成 {} 組 · {:.1}s · 最佳 {} → CAGR {} / MaxDD {}",
                            rows.len(),
                            t0.elapsed().as_secs_f32(),
                            best.label(),
                            pct(best.stats.cagr),
                            pct(best.stats.max_drawdown)
                        );
                        this.state.update(cx, |st, cx| st.set_status(StatusLevel::Ok, msg, cx));
                        this.progress.0 = this.progress.1;
                        this.rows = rows;
                        this.run_symbol = sym;
                        this.selected = Some(0);
                        this.boot = None;
                        this.wf = None;
                        this.rebuild_heat();
                    }
                    Ok(_) => this.state.update(cx, |st, cx| st.set_status(StatusLevel::Warn, "優化沒有產生任何有效結果（資料區間太短？）", cx)),
                    Err(e) => this.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, format!("優化失敗：{e}"), cx)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn rebuild_heat(&mut self) {
        if self.rows.is_empty() {
            self.heat = None;
            return;
        }
        let delta_of = |c: &RollConfig| match c.strike {
            StrikeSelection::Delta(d) => d,
            StrikeSelection::Moneyness(m) => m,
        };
        let mut deltas: Vec<f64> = self.rows.iter().map(|r| delta_of(&r.config)).collect();
        deltas.sort_by(f64::total_cmp);
        deltas.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        let mut dtes: Vec<u32> = self.rows.iter().map(|r| r.config.entry_dte).collect();
        dtes.sort();
        dtes.dedup();
        let mut vals = vec![vec![None; dtes.len()]; deltas.len()];
        let mut best_score = vec![vec![f64::NEG_INFINITY; dtes.len()]; deltas.len()];
        for r in &self.rows {
            let i = deltas.iter().position(|d| (d - delta_of(&r.config)).abs() < 1e-9).unwrap_or(0);
            let j = dtes.iter().position(|d| *d == r.config.entry_dte).unwrap_or(0);
            if r.score > best_score[i][j] {
                best_score[i][j] = r.score;
                vals[i][j] = Some(match self.metric {
                    HeatMetric::Score => r.score,
                    HeatMetric::Cagr => r.stats.cagr,
                    HeatMetric::MaxDd => r.stats.max_drawdown,
                    HeatMetric::Sharpe => r.stats.sharpe,
                });
            }
        }
        let best = &self.rows[self.selected.unwrap_or(0).min(self.rows.len() - 1)].config;
        let hi = (
            deltas.iter().position(|d| (d - delta_of(best)).abs() < 1e-9).unwrap_or(0),
            dtes.iter().position(|d| *d == best.entry_dte).unwrap_or(0),
        );
        let fmt: fn(f64) -> String = match self.metric {
            HeatMetric::Score => |v| format!("{v:.3}"),
            HeatMetric::Sharpe => |v| format!("{v:.2}"),
            _ => |v| format!("{:.1}%", v * 100.0),
        };
        self.heat = Some(Rc::new(HeatmapSpec {
            row_labels: deltas.iter().map(|d| format!("Δ{d:.2}")).collect(),
            col_labels: dtes.iter().map(|d| format!("{d}d")).collect(),
            values: vals,
            fmt,
            row_title: "Delta".into(),
            col_title: "進場 DTE (每格取其餘參數中分數最佳者)".into(),
            higher_better: true,
            highlight: Some(hi),
        }));
    }

    fn selected_row(&self) -> Option<&OptRow> {
        self.selected.and_then(|i| self.rows.get(i)).or(self.rows.first())
    }

    fn run_bootstrap(&mut self, cx: &mut Context<Self>) {
        if self.boot_running {
            return;
        }
        let Some(row) = self.selected_row() else {
            self.state.update(cx, |st, cx| st.set_status(StatusLevel::Warn, "請先執行優化並選擇一列", cx));
            return;
        };
        let cfg = row.config.clone();
        let label = row.label();
        let mut e = FormErrors::default();
        let n = e.int("boot_n", &self.inp.boot_n, 10, 5000, cx) as usize;
        let block = e.num_range("boot_block", &self.inp.boot_block, 1.0, 1000.0, cx);
        if !e.is_empty() {
            self.errors = e;
            cx.notify();
            return;
        }
        let sym = self.run_symbol.clone();
        let q = data_bridge::default_dividend_yield(&sym);
        self.boot_running = true;
        let task = self.state.update(cx, |st, cx| st.begin_task(format!("拔靴 {n} 次"), cx));
        let job = cx.background_executor().spawn(async move {
            let (data, _) = data_bridge::backtest_data(&sym, q)?;
            bootstrap_robustness(&data, &cfg, n, block, 7).map_err(|e| format!("{e:#}"))
        });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                this.boot_running = false;
                this.state.update(cx, |st, cx| st.end_task(task, cx));
                match res {
                    Ok(r) => {
                        let p = palette();
                        // histogram of CAGR samples
                        let mut s: Vec<f64> = r.cagr_samples.iter().copied().filter(|v| v.is_finite()).collect();
                        s.sort_by(f64::total_cmp);
                        let (lo, hi) = (s.first().copied().unwrap_or(0.0), s.last().copied().unwrap_or(0.0));
                        let bins = 30usize;
                        let w = ((hi - lo) / bins as f64).max(1e-6);
                        let mut counts = vec![0usize; bins];
                        for v in &s {
                            counts[(((v - lo) / w) as usize).min(bins - 1)] += 1;
                        }
                        let n = s.len().max(1) as f64;
                        let pts: Vec<(f64, f64)> = counts.iter().enumerate().map(|(i, c)| (lo + w * (i as f64 + 0.5), *c as f64 / n)).collect();
                        let spec = ChartSpec::new()
                            .x_fmt(fmt_pct)
                            .y_fmt(fmt_pct)
                            .series(Series::bars("CAGR 分佈", pts, p.blue, w).signed_colors())
                            .vline(RefLine::new(r.bh_cagr.p50, p.muted).label("買進持有 P50"))
                            .vline(RefLine::new(r.cagr.p50, p.yellow).label("策略 P50"))
                            .no_legend()
                            .into_rc();
                        this.state.update(cx, |st, cx| {
                            st.set_status(StatusLevel::Ok, format!("拔靴完成：CAGR P50 {} · 歸零機率 {}", pct(r.cagr.p50), pct(r.prob_wipeout)), cx)
                        });
                        this.boot = Some((r, spec, label));
                    }
                    Err(e) => this.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, format!("拔靴失敗：{e}"), cx)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn run_walk_forward(&mut self, cx: &mut Context<Self>) {
        if self.wf_running {
            return;
        }
        let Some((sym, base, grid, obj)) = self.build(cx) else {
            cx.notify();
            return;
        };
        let mut e = FormErrors::default();
        let train = e.num_range("wf_train", &self.inp.wf_train, 1.0, 30.0, cx);
        let test = e.num_range("wf_test", &self.inp.wf_test, 0.5, 20.0, cx);
        if !e.is_empty() {
            self.errors = e;
            cx.notify();
            return;
        }
        // keep walk-forward tractable: cap the grid
        let grid = if grid.len(&base) > 200 { ParamGrid::small() } else { grid };
        let q = data_bridge::default_dividend_yield(&sym);
        self.wf_running = true;
        let task = self.state.update(cx, |st, cx| st.begin_task("Walk-forward", cx));
        let job = cx.background_executor().spawn(async move {
            let (data, _) = data_bridge::backtest_data(&sym, q)?;
            let w = walk_forward(&data, &base, &grid, obj, train, test).map_err(|e| format!("{e:#}"))?;
            let s = walk_forward_summary(&w);
            Ok::<_, String>((w, s))
        });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                this.wf_running = false;
                this.state.update(cx, |st, cx| st.end_task(task, cx));
                match res {
                    Ok(v) => {
                        this.state.update(cx, |st, cx| st.set_status(StatusLevel::Ok, format!("Walk-forward 完成：{} 個視窗 · OOS CAGR {}", v.0.len(), pct(v.1.oos_cagr)), cx));
                        this.wf = Some(v);
                    }
                    Err(e) => this.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, format!("Walk-forward 失敗：{e}"), cx)),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn load_into_backtest(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(r) = self.rows.get(i) else { return };
        let patch = BacktestParamsPatch { symbol: Some(self.run_symbol.clone()), config: Some(r.config.clone()), run: true };
        self.state.update(cx, |_, cx| cx.emit(AppEvent::ApplyBacktestParams(Box::new(patch))));
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let e = &self.errors;
        let i = &self.inp;
        let p = palette();
        let row = || h_flex().gap_2().items_start();
        let f = |label: &str, key: &str, inp: &Entity<InputState>| field_err(label.to_string(), inp, e.get(key), cx).flex_1();
        // live combo count
        let combos = {
            let mut tmp = FormErrors::default();
            let g = ParamGrid {
                deltas: tmp.list("d", &i.deltas, cx),
                entry_dtes: tmp.list("e", &i.entry, cx).into_iter().map(|v| v as u32).collect(),
                roll_dtes: tmp.list("r", &i.roll, cx).into_iter().map(|v| v as u32).collect(),
                exposures: tmp.list("x", &i.exposure, cx),
                roll_up_deltas: parse_opt_list(&input_text(&i.roll_up, cx)).unwrap_or_default(),
                rebalance_bands: vec![],
                min_hold_days: parse_num(&input_text(&i.min_hold, cx)).unwrap_or(0.0) as u32,
            };
            if tmp.is_empty() { Some(g.len(&RollConfig::default())) } else { None }
        };
        v_flex()
            .gap_2()
            .p_2()
            .child(row().child(labeled("標的", Select::new(&self.symbol).small(), cx).flex_1()).child(f("起始日", "start", &i.start)).child(f("結束日", "end", &i.end)))
            .child(section_title("參數網格 (逗號分隔，或 起:迄:步長)", cx))
            .child(f("Delta", "deltas", &i.deltas))
            .child(row().child(f("進場 DTE", "entry", &i.entry)).child(f("滾動 DTE", "roll", &i.roll)))
            .child(row().child(f("目標曝險 x", "exposure", &i.exposure)).child(f("Roll-up Δ (無=不用)", "roll_up", &i.roll_up)))
            .child(row().child(f("最短持有天數", "min_hold", &i.min_hold)).child(f("顯示前 N 名", "top_n", &i.top_n)))
            .child(muted_text(format!("組合數：{}", combos.map(|n| n.to_string()).unwrap_or_else(|| "—".into())), cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(chip("robust-on", "穩健優化（拔靴中位數）", self.robust, cx).on_click(cx.listener(|this, _, _, cx| {
                        this.robust = true;
                        cx.notify();
                    })))
                    .child(chip("robust-off", "僅歷史路徑", !self.robust, cx).on_click(cx.listener(|this, _, _, cx| {
                        this.robust = false;
                        cx.notify();
                    }))),
            )
            .child(muted_text(if self.robust { "穩健模式：每組參數在 12 條區塊拔靴合成歷史 + 真實歷史上評分，取中位數並與相鄰格平滑，降低過度擬合。" } else { "僅以單一真實歷史評分（較快，但容易過度擬合）。" }, cx))
            .child(section_title("目標函數 Objective", cx))
            .child(h_flex().gap_1().flex_wrap().children(Obj::ALL.iter().map(|o| {
                let o = *o;
                chip(SharedString::from(format!("obj-{}", o.label())), o.label(), self.obj == o, cx).on_click(cx.listener(move |this, _, _, cx| {
                    this.obj = o;
                    cx.notify();
                }))
            })))
            .when(self.obj == Obj::Utility, |d| d.child(f("λ (回撤懲罰)", "lambda", &i.lambda)))
            .when(self.obj == Obj::DdCap, |d| d.child(f("最大回撤上限 %", "dd_cap", &i.dd_cap)))
            .child(
                Button::new("run-opt")
                    .primary()
                    .small()
                    .label(if self.running { "取消優化" } else { "執行優化 (Ctrl+Enter)" })
                    .on_click(cx.listener(|this, _, w, cx| this.run(w, cx))),
            )
            .when(self.running || self.progress.1 > 0, |d| {
                let (k, n) = self.progress;
                let v = if n > 0 { k as f32 / n as f32 * 100.0 } else { 0.0 };
                d.child(v_flex().gap_1().child(Progress::new().value(v)).child(muted_text(format!("{k} / {n} 組 ({v:.0}%)"), cx)))
            })
            .child(section_title("穩健性 Robustness (選定列)", cx))
            .child(row().child(f("拔靴次數", "boot_n", &i.boot_n)).child(f("平均區塊天數", "boot_block", &i.boot_block)))
            .child(Button::new("run-boot").small().label(if self.boot_running { "拔靴中…" } else { "執行區塊拔靴" }).loading(self.boot_running).on_click(cx.listener(|this, _, _, cx| this.run_bootstrap(cx))))
            .child(section_title("Walk-forward 樣本外驗證", cx))
            .child(row().child(f("訓練年數", "wf_train", &i.wf_train)).child(f("測試年數", "wf_test", &i.wf_test)))
            .child(Button::new("run-wf").small().label(if self.wf_running { "執行中…" } else { "執行 Walk-forward" }).loading(self.wf_running).on_click(cx.listener(|this, _, _, cx| this.run_walk_forward(cx))))
            .child(muted_text("網格 > 200 組時 Walk-forward 自動改用精簡網格 (36 組)。", cx))
            .when(!e.is_empty(), |d| d.child(div().text_size(px(11.0)).text_color(p.red).child(e.summary())))
    }

    fn render_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let n = parse_num(&input_text(&self.inp.top_n, cx)).unwrap_or(30.0).clamp(1.0, 2000.0) as usize;
        let cols = vec![
            Col::new("#", 32.0),
            Col::new("履約", 56.0),
            Col::new("進場", 48.0),
            Col::new("滾動", 48.0),
            Col::new("曝險", 48.0),
            Col::new("Roll-up", 60.0),
            Col::new("CAGR", 66.0),
            Col::new("MaxDD", 66.0),
            Col::new("Sharpe", 56.0),
            Col::new("Calmar", 56.0),
            Col::new("分數", 64.0),
            Col::new("滾動數", 56.0),
            Col::new("最低權益", 66.0),
            Col::new("成本", 76.0),
            Col::new("", 70.0).center(),
        ];
        let w = table_width(&cols);
        v_flex()
            .child(table_header(&cols, cx).w(px(w)))
            .child(div().id("opt-rows").max_h(px(360.0)).overflow_y_scroll().child(v_flex().w(px(w)).children(self.rows.iter().take(n).enumerate().map(|(i, r)| {
                let c = &r.config;
                let k = match c.strike {
                    StrikeSelection::Delta(d) => format!("Δ{d:.2}"),
                    StrikeSelection::Moneyness(m) => format!("K/S{m:.2}"),
                };
                let sel = self.selected == Some(i);
                let row = table_row(
                    SharedString::from(format!("opt-{i}")),
                    &cols,
                    vec![
                        Cell::new((i + 1).to_string()).color(p.dim),
                        Cell::new(k),
                        Cell::new(c.entry_dte.to_string()),
                        Cell::new(c.roll_dte.to_string()),
                        Cell::new(format!("{:.2}x", c.target_exposure)),
                        Cell::new(c.roll_up_delta.map(|u| format!("{u:.2}")).unwrap_or_else(|| "—".into())),
                        Cell::new(pct(r.stats.cagr)).color(sign_color(r.stats.cagr)),
                        Cell::new(pct(r.stats.max_drawdown)).color(p.red),
                        Cell::new(f2(r.stats.sharpe)),
                        Cell::new(f2(r.stats.calmar)),
                        Cell::new(format!("{:.3}", r.score)).color(p.yellow),
                        Cell::new(r.n_rolls.to_string()),
                        Cell::new(pct(r.min_equity_ratio)).color(if r.near_wipe { p.red } else { p.muted }),
                        Cell::new(money(r.total_costs)).color(p.muted),
                        Cell::new(if r.wiped_out { "歸零" } else if r.near_wipe { "近乎歸零" } else { "→ 回測" }).color(if r.near_wipe { p.red } else { p.blue }),
                    ],
                    i % 2 == 1,
                    cx,
                )
                .cursor_pointer()
                .when(sel, |d| d.bg(p.blue.opacity(0.18)))
                .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| {
                    this.selected = Some(i);
                    this.rebuild_heat();
                    if ev.click_count() >= 2 {
                        this.load_into_backtest(i, cx);
                    }
                    cx.notify();
                }));
                row
            }))))
    }

    fn render_boot(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some((r, spec, label)) = &self.boot else {
            return panel("區塊拔靴穩健性 Bootstrap", cx).child(empty_state(if self.boot_running { "背景模擬中…" } else { "選擇一列後按「執行區塊拔靴」：以歷史 (報酬, 波動) 配對重抽樣產生合成歷史並重跑回測" }, cx)).into_any_element();
        };
        let cols = vec![Col::new("百分位", 90.0).left(), Col::new("P5", 64.0), Col::new("P25", 64.0), Col::new("P50", 64.0), Col::new("P75", 64.0), Col::new("P95", 64.0), Col::new("平均", 64.0)];
        let prow = |id: &str, name: &str, x: &Percentiles, f: fn(f64) -> String, even: bool| {
            table_row(SharedString::from(id.to_string()), &cols, vec![Cell::new(name.to_string()).color(p.muted), Cell::new(f(x.p5)), Cell::new(f(x.p25)), Cell::new(f(x.p50)), Cell::new(f(x.p75)), Cell::new(f(x.p95)), Cell::new(f(x.mean))], even, cx)
        };
        panel(format!("區塊拔靴穩健性 · {label} · {} 次 · 區塊 {:.0} 天 · {:.1} 年", r.n_samples, r.block_days, r.years), cx)
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .p_2()
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                v_flex()
                                    .child(table_header(&cols, cx))
                                    .child(prow("b1", "策略 CAGR", &r.cagr, pct, false))
                                    .child(prow("b2", "策略 MaxDD", &r.max_dd, pct, true))
                                    .child(prow("b3", "策略 Sharpe", &r.sharpe, f2, false))
                                    .child(prow("b4", "買進持有 CAGR", &r.bh_cagr, pct, true))
                                    .child(prow("b5", "買進持有 MaxDD", &r.bh_max_dd, pct, false))
                                    .child(prow("b6", "槓桿ETF CAGR", &r.lev_cagr, pct, true))
                                    .child(prow("b7", "槓桿ETF MaxDD", &r.lev_max_dd, pct, false))
                                    .child(prow("b8", "曝險配對線性 CAGR", &r.matched_cagr, pct, true))
                                    .child(prow("b9", "最低權益/初始", &r.min_equity, pct, false)),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
                                    .child(kpi("歸零機率", pct(r.prob_wipeout), Some(p.red), cx))
                                    .child(kpi("勝過買進持有", pct(r.prob_beat_benchmark), Some(p.green), cx))
                                    .child(kpi("勝過槓桿ETF", pct(r.prob_beat_lev_etf), Some(p.orange), cx))
                                    .child(kpi("虧損機率", pct(r.prob_loss), None, cx))
                                    .child(kpi("回撤 > 50%", pct(r.prob_dd_over_50), None, cx))
                                    .child(kpi("近乎歸零機率", pct(r.prob_near_wipe), Some(p.red), cx))
                                    .child(kpi("勝過曝險配對線性", pct(r.prob_beat_matched), Some(p.cyan), cx)),
                            ),
                    )
                    .child(div().flex_1().min_w(px(240.0)).h(px(250.0)).child(xy_chart(spec.clone()))),
            )
            .into_any_element()
    }

    fn render_wf(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some((w, s)) = &self.wf else {
            return panel("Walk-forward 樣本外驗證", cx).child(empty_state(if self.wf_running { "背景執行中…" } else { "滾動「訓練期最佳化 → 下一段測試」，檢驗參數是否過度擬合" }, cx)).into_any_element();
        };
        let cols = vec![
            Col::new("訓練期", 170.0).left(),
            Col::new("測試期", 170.0).left(),
            Col::new("最佳參數", 190.0).left(),
            Col::new("IS CAGR", 70.0),
            Col::new("OOS CAGR", 76.0),
            Col::new("基礎 OOS", 76.0),
            Col::new("買進持有", 70.0),
            Col::new("OOS DD", 66.0),
        ];
        let tw = table_width(&cols);
        panel(format!("Walk-forward · {} 個視窗 · {:.1} 年", w.len(), s.years), cx).child(
            v_flex()
                .p_2()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(kpi("OOS 串接 CAGR", pct(s.oos_cagr), Some(sign_color(s.oos_cagr)), cx))
                        .child(kpi("基礎參數 CAGR", pct(s.base_cagr), None, cx))
                        .child(kpi("買進持有 CAGR", pct(s.bh_cagr), Some(p.muted), cx))
                        .child(kpi("最差 OOS DD", pct(s.worst_oos_dd), Some(p.red), cx))
                        .child(kpi("最佳化勝過基礎", pct(s.pct_windows_opt_beats_base), None, cx)),
                )
                .child(div().id("wf-h").overflow_x_scroll().child(v_flex().w(px(tw)).child(table_header(&cols, cx)).children(w.iter().enumerate().map(|(i, x)| {
                    table_row(
                        SharedString::from(format!("wf-{i}")),
                        &cols,
                        vec![
                            Cell::new(format!("{} → {}", x.train_start, x.train_end)),
                            Cell::new(format!("{} → {}", x.test_start, x.test_end)),
                            Cell::new(x.best_config.label()).color(p.blue),
                            Cell::new(pct(x.in_sample_stats.cagr)),
                            Cell::new(pct(x.oos_stats.cagr)).color(sign_color(x.oos_stats.cagr)),
                            Cell::new(pct(x.base_oos_stats.cagr)),
                            Cell::new(pct(x.bh_oos_stats.cagr)).color(p.muted),
                            Cell::new(pct(x.oos_stats.max_drawdown)).color(p.red),
                        ],
                        i % 2 == 1,
                        cx,
                    )
                })))),
        )
        .into_any_element()
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        if self.rows.is_empty() {
            return v_flex()
                .gap_2()
                .child(empty_state(if self.running { "優化執行中（背景 + 多核心）…" } else { "設定網格與目標函數後按「執行優化」。點選一列可查看熱圖位置；雙擊或點「→ 回測」載入回測頁。" }, cx))
                .child(self.render_boot(cx).into_any_element())
                .child(self.render_wf(cx).into_any_element())
                .into_any_element();
        }
        let metric_chips = h_flex().gap_1().children(
            [(HeatMetric::Score, "分數"), (HeatMetric::Cagr, "CAGR"), (HeatMetric::MaxDd, "MaxDD"), (HeatMetric::Sharpe, "Sharpe")].into_iter().map(|(m, l)| {
                chip(SharedString::from(format!("hm-{l}")), l, self.metric == m, cx).on_click(cx.listener(move |this, _, _, cx| {
                    this.metric = m;
                    this.rebuild_heat();
                    cx.notify();
                }))
            }),
        );
        let sel = self.selected_row().cloned();
        v_flex()
            .gap_2()
            .child(
                panel_with(
                    format!("{} 優化結果 · {} 組 · 點選選取，雙擊 / 「→ 回測」載入", self.run_symbol, self.rows.len()),
                    h_flex().gap_1().when_some(self.selected, |d, i| {
                        d.child(chip("to-bt", "載入選定列到回測 →", false, cx).on_click(cx.listener(move |this, _, _, cx| this.load_into_backtest(i, cx))))
                    }),
                    cx,
                )
                .child(div().id("opt-h").overflow_x_scroll().child(self.render_table(cx))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(panel_with("Delta × 進場 DTE 熱圖", metric_chips, cx).flex_1().h(px(300.0)).child(div().flex_1().min_h_0().p_1().children(self.heat.clone().map(heatmap))))
                    .when_some(sel, |d, r| {
                        d.child(
                            panel("選定參數", cx).w(px(260.0)).flex_none().child(
                                panel_body()
                                    .child(kv("參數", r.label(), cx))
                                    .child(kv_colored("最低權益/初始", pct(r.min_equity_ratio), if r.near_wipe { p.red } else { p.text }, cx))
                                    .child(kv_colored("CAGR", pct2(r.stats.cagr), sign_color(r.stats.cagr), cx))
                                    .child(kv_colored("MaxDD", pct(r.stats.max_drawdown), p.red, cx))
                                    .child(kv("Sharpe / Sortino", format!("{} / {}", f2(r.stats.sharpe), f2(r.stats.sortino)), cx))
                                    .child(kv("Calmar", f2(r.stats.calmar), cx))
                                    .child(kv("年化波動", pct(r.stats.ann_vol), cx))
                                    .child(kv("平均曝險", format!("{:.2}x", r.avg_exposure), cx))
                                    .child(kv("時間價值/年", pct2(r.avg_extrinsic_pct_annual), cx))
                                    .child(kv("滾動次數", r.n_rolls.to_string(), cx))
                                    .child(kv("期末價值", money(r.stats.final_value), cx)),
                            ),
                        )
                    }),
            )
            .child(self.render_boot(cx).into_any_element())
            .child(self.render_wf(cx).into_any_element())
            .into_any_element()
    }
}

impl Render for OptimizerPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self.render_form(cx);
        let results = self.render_results(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(div().id("opt-form").w(px(380.0)).flex_none().h_full().overflow_y_scroll().border_r_1().border_color(palette().border).child(form))
            .child(div().id("opt-results").flex_1().min_w_0().h_full().overflow_y_scroll().p_2().child(results))
    }
}
