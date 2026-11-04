//! LEAPS 滾動回測: RollConfig form → background run → equity / drawdown / exposure
//! charts, stats vs benchmarks, yearly returns and trade log.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Subscription, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::InputState,
    select::{Select, SelectEvent, SelectState},
    v_flex, IndexPath, Sizable,
};
use optlab_core::backtest::{run_backtest, BacktestResult, RollConfig, StrikeSelection, TradeAction};
use optlab_core::stats::{drawdown_series, PerformanceStats};

use crate::data_bridge;
use crate::state::{AppState, StatusLevel};
use crate::theme::palette;
use crate::widgets::charts::{date_to_x, fmt_money, fmt_pct, xy_chart, ChartSpec, RefLine, Series};
use crate::widgets::ui::*;

/// Parameters pushed from other pages (optimiser row, dashboard symbol).
#[derive(Clone, Debug, Default)]
pub struct BacktestParamsPatch {
    pub symbol: Option<String>,
    pub config: Option<RollConfig>,
    pub run: bool,
}

struct Inputs {
    start: Entity<InputState>,
    end: Entity<InputState>,
    capital: Entity<InputState>,
    strike_val: Entity<InputState>,
    entry_dte: Entity<InputState>,
    roll_dte: Entity<InputState>,
    roll_up: Entity<InputState>,
    roll_down: Entity<InputState>,
    profit_take: Entity<InputState>,
    exposure: Entity<InputState>,
    band: Entity<InputState>,
    max_prem: Entity<InputState>,
    half_spread: Entity<InputState>,
    commission: Entity<InputState>,
    slippage: Entity<InputState>,
    q: Entity<InputState>,
    vol_scale: Entity<InputState>,
    rate_spread: Entity<InputState>,
    lev_spread: Entity<InputState>,
    lev_er: Entity<InputState>,
}

pub struct BacktestPage {
    state: Entity<AppState>,
    symbol: Entity<SelectState<Vec<SharedString>>>,
    inp: Inputs,
    by_delta: bool,
    integer: bool,
    cash_yield: bool,
    log: bool,
    errors: FormErrors,
    running: bool,
    result: Option<Arc<BacktestResult>>,
    result_symbol: String,
    vol_name: String,
    charts: Option<Charts>,
    trades_shown: usize,
    _subs: Vec<Subscription>,
}

struct Charts {
    equity: Rc<ChartSpec>,
    equity_log: Rc<ChartSpec>,
    drawdown: Rc<ChartSpec>,
    exposure: Rc<ChartSpec>,
    yearly: Rc<ChartSpec>,
}

fn opt_str(v: Option<f64>, scale: f64) -> String {
    v.map(|x| format!("{}", (x * scale * 1000.0).round() / 1000.0)).unwrap_or_default()
}

impl BacktestPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let syms: Vec<SharedString> = data_bridge::bundled_symbols().into_iter().map(SharedString::from).collect();
        let default = state.read(cx).settings.default_symbol.to_ascii_uppercase();
        let idx = syms.iter().position(|s| s.as_ref() == default).unwrap_or(0);
        let symbol = cx.new(|cx| SelectState::new(syms, Some(IndexPath::default().row(idx)), window, cx));
        let sym = default.clone();
        let cfg = Self::default_config(&sym);
        let q = data_bridge::default_dividend_yield(&sym);
        let i = |v: String, window: &mut Window, cx: &mut Context<Self>| new_input(v, window, cx);
        let inp = Inputs {
            start: new_input_ph("2010-01-01", "YYYY-MM-DD (空白=全部)", window, cx),
            end: new_input_ph("", "YYYY-MM-DD (空白=最新)", window, cx),
            capital: i(format!("{}", cfg.initial_capital), window, cx),
            strike_val: i("0.80".into(), window, cx),
            entry_dte: i(cfg.entry_dte.to_string(), window, cx),
            roll_dte: i(cfg.roll_dte.to_string(), window, cx),
            roll_up: new_input_ph("", "例 0.95 (空白=不用)", window, cx),
            roll_down: new_input_ph("", "例 0.55", window, cx),
            profit_take: new_input_ph("", "例 100 (%)", window, cx),
            exposure: i(format!("{}", cfg.target_exposure), window, cx),
            band: i(opt_str(cfg.rebalance_band, 100.0), window, cx),
            max_prem: i(format!("{}", cfg.max_premium_pct * 100.0), window, cx),
            half_spread: i(format!("{}", cfg.costs.half_spread_pct * 100.0), window, cx),
            commission: i(format!("{}", cfg.costs.commission_per_contract), window, cx),
            slippage: i(format!("{}", cfg.costs.slippage * 100.0), window, cx),
            q: i(format!("{}", q * 100.0), window, cx),
            vol_scale: i("1.0".into(), window, cx),
            rate_spread: i(format!("{}", cfg.option_rate_spread * 100.0), window, cx),
            lev_spread: i(format!("{}", cfg.lev_borrow_spread * 100.0), window, cx),
            lev_er: i(format!("{}", cfg.lev_expense_ratio * 100.0), window, cx),
        };
        let subs = vec![cx.subscribe_in(&symbol, window, |this, _, ev: &SelectEvent<Vec<SharedString>>, window, cx| {
            let SelectEvent::Confirm(Some(v)) = ev else { return };
            let q = data_bridge::default_dividend_yield(v);
            this.inp.q.update(cx, |s, cx| s.set_value(format!("{}", q * 100.0), window, cx));
            let cfg = Self::default_config(v);
            this.set_costs(&cfg, window, cx);
        })];
        Self {
            state,
            symbol,
            inp,
            by_delta: true,
            integer: cfg.integer_contracts,
            cash_yield: cfg.cash_yield,
            log: true,
            errors: FormErrors::default(),
            running: false,
            result: None,
            result_symbol: String::new(),
            vol_name: String::new(),
            charts: None,
            trades_shown: 200,
            _subs: subs,
        }
    }

    fn default_config(sym: &str) -> RollConfig {
        match sym.to_ascii_uppercase().as_str() {
            "SPY" | "VTI" => RollConfig::spy(),
            _ => RollConfig::qqq(),
        }
    }

    fn set_costs(&mut self, cfg: &RollConfig, window: &mut Window, cx: &mut Context<Self>) {
        let set = |e: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| {
            e.update(cx, |s, cx| s.set_value(v, window, cx));
        };
        set(&self.inp.half_spread, format!("{}", cfg.costs.half_spread_pct * 100.0), window, cx);
        set(&self.inp.commission, format!("{}", cfg.costs.commission_per_contract), window, cx);
        set(&self.inp.slippage, format!("{}", cfg.costs.slippage * 100.0), window, cx);
    }

    fn symbol_str(&self, cx: &Context<Self>) -> String {
        self.symbol.read(cx).selected_value().map(|s| s.to_string()).unwrap_or_else(|| "QQQ".into())
    }

    /// Fill the form from a config (used by optimiser → backtest).
    pub fn apply_patch(&mut self, patch: &BacktestParamsPatch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sym) = &patch.symbol {
            let sym: SharedString = sym.to_ascii_uppercase().into();
            self.symbol.update(cx, |s, cx| s.set_selected_value(&sym, window, cx));
            let q = data_bridge::default_dividend_yield(&sym);
            self.inp.q.update(cx, |s, cx| s.set_value(format!("{}", q * 100.0), window, cx));
        }
        if let Some(c) = &patch.config {
            let set = |e: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| {
                e.update(cx, |s, cx| s.set_value(v, window, cx));
            };
            match c.strike {
                StrikeSelection::Delta(d) => {
                    self.by_delta = true;
                    set(&self.inp.strike_val, format!("{d}"), window, cx);
                }
                StrikeSelection::Moneyness(m) => {
                    self.by_delta = false;
                    set(&self.inp.strike_val, format!("{m}"), window, cx);
                }
            }
            set(&self.inp.capital, format!("{}", c.initial_capital), window, cx);
            set(&self.inp.entry_dte, c.entry_dte.to_string(), window, cx);
            set(&self.inp.roll_dte, c.roll_dte.to_string(), window, cx);
            set(&self.inp.roll_up, opt_str(c.roll_up_delta, 1.0), window, cx);
            set(&self.inp.roll_down, opt_str(c.roll_down_delta, 1.0), window, cx);
            set(&self.inp.profit_take, opt_str(c.profit_take_pct, 100.0), window, cx);
            set(&self.inp.exposure, format!("{}", c.target_exposure), window, cx);
            set(&self.inp.band, opt_str(c.rebalance_band.filter(|b| *b > 0.0), 100.0), window, cx);
            set(&self.inp.max_prem, format!("{}", c.max_premium_pct * 100.0), window, cx);
            set(&self.inp.start, c.start.map(|d| d.to_string()).unwrap_or_default(), window, cx);
            set(&self.inp.end, c.end.map(|d| d.to_string()).unwrap_or_default(), window, cx);
            self.set_costs(c, window, cx);
            set(&self.inp.rate_spread, format!("{}", c.option_rate_spread * 100.0), window, cx);
            self.integer = c.integer_contracts;
            self.cash_yield = c.cash_yield;
        }
        cx.notify();
        if patch.run {
            self.run(window, cx);
        }
    }

    /// Parse the form; Err = inline errors are set.
    pub fn build_config(&mut self, cx: &mut Context<Self>) -> Option<(String, RollConfig, f64)> {
        let sym = self.symbol_str(cx);
        let mut e = FormErrors::default();
        let i = &self.inp;
        let mut cfg = Self::default_config(&sym);
        cfg.start = e.date("start", &i.start, cx);
        cfg.end = e.date("end", &i.end, cx);
        cfg.initial_capital = e.num_range("capital", &i.capital, 1000.0, 1e10, cx);
        let sv = e.num_range("strike_val", &i.strike_val, 0.05, if self.by_delta { 0.99 } else { 2.0 }, cx);
        cfg.strike = if self.by_delta { StrikeSelection::Delta(sv) } else { StrikeSelection::Moneyness(sv) };
        cfg.entry_dte = e.int("entry_dte", &i.entry_dte, 20, 1200, cx) as u32;
        cfg.roll_dte = e.int("roll_dte", &i.roll_dte, 0, 1100, cx) as u32;
        if cfg.roll_dte >= cfg.entry_dte {
            e.errors.push(("roll_dte", "滾動 DTE 必須小於進場 DTE".into()));
        }
        let opt = |e: &mut FormErrors, key: &'static str, inp: &Entity<InputState>, lo: f64, hi: f64, scale: f64| -> Option<f64> {
            if input_text(inp, cx).is_empty() {
                None
            } else {
                let v = e.num_range(key, inp, lo, hi, cx);
                v.is_finite().then_some(v / scale)
            }
        };
        cfg.roll_up_delta = opt(&mut e, "roll_up", &i.roll_up, 0.3, 1.0, 1.0);
        cfg.roll_down_delta = opt(&mut e, "roll_down", &i.roll_down, 0.05, 0.95, 1.0);
        cfg.profit_take_pct = opt(&mut e, "profit_take", &i.profit_take, 1.0, 10000.0, 100.0);
        cfg.target_exposure = e.num_range("exposure", &i.exposure, 0.05, 10.0, cx);
        cfg.rebalance_band = opt(&mut e, "band", &i.band, 0.0, 500.0, 100.0);
        cfg.max_premium_pct = e.num_range("max_prem", &i.max_prem, 1.0, 100.0, cx) / 100.0;
        cfg.costs.half_spread_pct = e.num_range("half_spread", &i.half_spread, 0.0, 50.0, cx) / 100.0;
        cfg.costs.commission_per_contract = e.num_range("commission", &i.commission, 0.0, 100.0, cx);
        cfg.costs.slippage = e.num_range("slippage", &i.slippage, 0.0, 50.0, cx) / 100.0;
        let q = e.num_range("q", &i.q, 0.0, 20.0, cx) / 100.0;
        let vs = e.num_range("vol_scale", &i.vol_scale, 0.3, 3.0, cx);
        if vs.is_finite() {
            cfg.vol_model.iv_scale *= vs;
            cfg.vol_model.long_run_vol *= vs;
        }
        cfg.option_rate_spread = e.num_range("rate_spread", &i.rate_spread, -5.0, 10.0, cx) / 100.0;
        cfg.lev_borrow_spread = e.num_range("lev_spread", &i.lev_spread, -5.0, 20.0, cx) / 100.0;
        cfg.lev_expense_ratio = e.num_range("lev_er", &i.lev_er, 0.0, 10.0, cx) / 100.0;
        cfg.integer_contracts = self.integer;
        cfg.cash_yield = self.cash_yield;
        if let (Some(a), Some(b)) = (cfg.start, cfg.end) {
            if b <= a {
                e.errors.push(("end", "結束日須晚於起始日".into()));
            }
        }
        let ok = e.is_empty();
        self.errors = e;
        ok.then_some((sym, cfg, q))
    }

    pub fn run(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }
        let Some((sym, cfg, q)) = self.build_config(cx) else {
            let msg = format!("參數錯誤：{}", self.errors.summary());
            self.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, msg, cx));
            cx.notify();
            return;
        };
        self.running = true;
        let task_id = self.state.update(cx, |st, cx| {
            st.set_status(StatusLevel::Info, format!("回測 {sym} {} …", cfg.label()), cx);
            st.begin_task(format!("回測 {sym}"), cx)
        });
        let sym2 = sym.clone();
        let t0 = std::time::Instant::now();
        let job = cx.background_executor().spawn(async move {
            let (data, vname) = data_bridge::backtest_data(&sym2, q)?;
            let res = run_backtest(&data, &cfg).map_err(|e| format!("{e:#}"))?;
            Ok::<_, String>((res, vname))
        });
        cx.spawn(async move |this, cx| {
            let out = job.await;
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                this.state.update(cx, |st, cx| st.end_task(task_id, cx));
                match out {
                    Ok((res, vname)) => {
                        let msg = format!(
                            "回測完成 {sym} · CAGR {} · MaxDD {} · {} 筆交易 · {:.1}s",
                            pct(res.stats.cagr),
                            pct(res.stats.max_drawdown),
                            res.trades.len(),
                            t0.elapsed().as_secs_f32()
                        );
                        let lvl = if res.wiped_out { StatusLevel::Warn } else { StatusLevel::Ok };
                        this.state.update(cx, |st, cx| st.set_status(lvl, msg, cx));
                        this.charts = Some(Self::make_charts(&res));
                        this.result = Some(Arc::new(res));
                        this.result_symbol = sym;
                        this.vol_name = vname;
                    }
                    Err(e) => {
                        this.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, format!("回測失敗：{e}"), cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn make_charts(res: &BacktestResult) -> Charts {
        let p = palette();
        let eq: Vec<(f64, f64)> = res.records.iter().map(|r| (date_to_x(r.date), r.equity)).collect();
        let colors = [p.blue, p.muted, p.orange, p.purple];
        let mut equity = ChartSpec::new().dates().y_fmt(fmt_money).series(Series::line("LEAPS 滾動", eq.clone(), colors[0]).width(1.8));
        for (i, (name, c)) in res.benchmarks.iter().enumerate() {
            let pts = c.iter().map(|(d, v)| (date_to_x(*d), *v)).collect();
            equity = equity.series(Series::line(name.clone(), pts, colors[(i + 1) % colors.len()]).width(1.1));
        }
        equity = equity.hline(RefLine::new(res.config.initial_capital, p.dim).label("初始資金"));
        let equity_log = Rc::new(equity.clone().log_y(true));
        let equity = Rc::new(equity);

        let curve = res.equity_curve();
        let dd: Vec<(f64, f64)> = drawdown_series(&curve).into_iter().map(|(d, v)| (date_to_x(d), v)).collect();
        let mut drawdown = ChartSpec::new().dates().y_fmt(fmt_pct).series(Series::area("LEAPS 回撤", dd, p.red, 0.0).width(1.2));
        if let Some((name, c)) = res.benchmarks.first() {
            let bdd = drawdown_series(c).into_iter().map(|(d, v)| (date_to_x(d), v)).collect();
            drawdown = drawdown.series(Series::line(format!("{name} 回撤"), bdd, p.muted).width(1.0));
        }
        let exp: Vec<(f64, f64)> = res.records.iter().map(|r| (date_to_x(r.date), r.exposure)).collect();
        let delta: Vec<(f64, f64)> = res.records.iter().filter(|r| r.contracts > 0.0).map(|r| (date_to_x(r.date), r.delta)).collect();
        let exposure = ChartSpec::new()
            .dates()
            .series(Series::line("曝險 (Δ名目/權益)", exp, p.green).width(1.2))
            .series(Series::line("單口 Delta", delta, p.yellow).width(1.0))
            .hline(RefLine::new(res.config.target_exposure, p.dim).label(format!("目標 {:.2}x", res.config.target_exposure)))
            .include_zero()
            .into_rc();

        // yearly returns: strategy vs buy & hold side by side
        let mut yearly = ChartSpec::new().y_fmt(fmt_pct).x_fmt(|v| format!("{v:.0}")).include_zero();
        let ys: Vec<(f64, f64)> = res.stats.yearly.iter().map(|(y, r)| (*y as f64 - 0.18, *r)).collect();
        yearly = yearly.series(Series::bars("LEAPS 滾動", ys, p.blue, 0.36));
        if let Some((name, bs)) = res.benchmark_stats.first() {
            let bh: Vec<(f64, f64)> = bs.yearly.iter().map(|(y, r)| (*y as f64 + 0.18, *r)).collect();
            yearly = yearly.series(Series::bars(name.clone(), bh, p.muted, 0.36));
        }
        Charts { equity, equity_log, drawdown: drawdown.into_rc(), exposure, yearly: yearly.into_rc() }
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let e = &self.errors;
        let i = &self.inp;
        let row = || h_flex().gap_2().items_start();
        let f = |label: &str, key: &str, inp: &Entity<InputState>| field_err(label.to_string(), inp, e.get(key), cx).flex_1();
        v_flex()
            .gap_2()
            .p_2()
            .child(row().child(labeled("標的 (內建資料)", Select::new(&self.symbol).small(), cx).flex_1()).child(f("初始資金 $", "capital", &i.capital)))
            .child(row().child(f("起始日", "start", &i.start)).child(f("結束日", "end", &i.end)))
            .child(section_title("履約價與到期", cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(chip("sk-delta", "依 Delta", self.by_delta, cx).on_click(cx.listener(|this, _, w, cx| {
                        this.by_delta = true;
                        this.inp.strike_val.update(cx, |s, cx| s.set_value("0.80", w, cx));
                        cx.notify();
                    })))
                    .child(chip("sk-mny", "依價內程度 K/S", !self.by_delta, cx).on_click(cx.listener(|this, _, w, cx| {
                        this.by_delta = false;
                        this.inp.strike_val.update(cx, |s, cx| s.set_value("0.85", w, cx));
                        cx.notify();
                    }))),
            )
            .child(
                row()
                    .child(f(if self.by_delta { "目標 Delta" } else { "K/S (0.85=價內15%)" }, "strike_val", &i.strike_val))
                    .child(f("進場 DTE (天)", "entry_dte", &i.entry_dte))
                    .child(f("滾動 DTE ≤", "roll_dte", &i.roll_dte)),
            )
            .child(row().child(f("Roll-up Δ >", "roll_up", &i.roll_up)).child(f("Roll-down Δ <", "roll_down", &i.roll_down)).child(f("獲利了結 %", "profit_take", &i.profit_take)))
            .child(section_title("部位與曝險", cx))
            .child(row().child(f("目標曝險 (x)", "exposure", &i.exposure)).child(f("再平衡帶 ±%", "band", &i.band)).child(f("權利金上限 %權益", "max_prem", &i.max_prem)))
            .child(
                h_flex()
                    .gap_3()
                    .child(Checkbox::new("int-contracts").label("整數口數").checked(self.integer).on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.integer = *v;
                        cx.notify();
                    })))
                    .child(Checkbox::new("cash-yield").label("閒置現金收息").checked(self.cash_yield).on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.cash_yield = *v;
                        cx.notify();
                    }))),
            )
            .child(section_title("成本與模型", cx))
            .child(row().child(f("半價差 %", "half_spread", &i.half_spread)).child(f("手續費 $/口", "commission", &i.commission)).child(f("滑價 %", "slippage", &i.slippage)))
            .child(row().child(f("股息率 q %", "q", &i.q)).child(f("波動模型倍數", "vol_scale", &i.vol_scale)).child(f("選擇權利差 %", "rate_spread", &i.rate_spread)))
            .child(row().child(f("槓桿ETF 借貸利差 %", "lev_spread", &i.lev_spread)).child(f("槓桿ETF 費用率 %", "lev_er", &i.lev_er)))
            .child(
                Button::new("run-bt")
                    .primary()
                    .small()
                    .label(if self.running { "回測中…" } else { "執行回測 (Ctrl+Enter)" })
                    .loading(self.running)
                    .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
            )
            .when(!self.errors.is_empty(), |d| d.child(div().text_size(px(11.0)).text_color(palette().red).child(self.errors.summary())))
            .child(muted_text(
                "說明：以內建歷史價格 + 波動率指數 (VXN/VIX) 驅動的偏斜 IV 曲面合成 LEAPS 價格；利率使用 3M 國庫券歷史。基準：買進持有與每日再平衡 L 倍槓桿 ETF。",
                cx,
            ))
    }

    fn stats_table(&self, res: &BacktestResult, cx: &mut Context<Self>) -> impl IntoElement {
        let mut cols = vec![Col::new("指標", 130.0).left(), Col::new("LEAPS 滾動", 110.0)];
        for (n, _) in &res.benchmark_stats {
            cols.push(Col::new(n.clone(), 150.0));
        }
        type F = fn(&PerformanceStats) -> String;
        let rows: Vec<(&str, F, bool)> = vec![
            ("期末價值", |s| money(s.final_value), false),
            ("總報酬", |s| pct(s.total_return), true),
            ("CAGR 年化報酬", |s| pct2(s.cagr), true),
            ("年化波動", |s| pct(s.ann_vol), false),
            ("Sharpe", |s| f2(s.sharpe), false),
            ("Sortino", |s| f2(s.sortino), false),
            ("最大回撤 MaxDD", |s| pct(s.max_drawdown), true),
            ("Calmar", |s| f2(s.calmar), false),
            ("Ulcer 指數", |s| pct(s.ulcer_index), false),
            ("最長水下 (天)", |s| s.longest_dd_days.to_string(), false),
            ("最佳年度", |s| s.best_year.map(|(y, r)| format!("{y} {}", pct(r))).unwrap_or_default(), false),
            ("最差年度", |s| s.worst_year.map(|(y, r)| format!("{y} {}", pct(r))).unwrap_or_default(), false),
            ("正報酬月份", |s| pct(s.pct_positive_months), false),
            ("最差單月", |s| pct(s.worst_month), true),
            ("最差單日", |s| pct(s.worst_day), true),
            ("日 VaR95", |s| pct2(s.var95), false),
            ("日 CVaR95", |s| pct2(s.cvar95), false),
        ];
        let mut all: Vec<&PerformanceStats> = vec![&res.stats];
        all.extend(res.benchmark_stats.iter().map(|(_, s)| s));
        let w = table_width(&cols);
        v_flex()
            .w(px(w))
            .child(table_header(&cols, cx))
            .children(rows.into_iter().enumerate().map(|(ri, (label, f, signed))| {
                let mut cells = vec![Cell::new(label).color(palette().muted)];
                for s in &all {
                    let txt = f(s);
                    let c = if signed {
                        let v = match label {
                            "總報酬" => s.total_return,
                            "CAGR 年化報酬" => s.cagr,
                            _ => -1.0,
                        };
                        Some(sign_color(v))
                    } else {
                        None
                    };
                    cells.push(Cell::new(txt).maybe_color(c));
                }
                table_row(SharedString::from(format!("st-{ri}")), &cols, cells, ri % 2 == 1, cx)
            }))
    }

    fn trades_table(&self, res: &BacktestResult, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let cols = vec![
            Col::new("日期", 86.0).left(),
            Col::new("動作", 64.0).left(),
            Col::new("履約價", 72.0),
            Col::new("到期", 86.0),
            Col::new("口數", 64.0),
            Col::new("價格", 70.0),
            Col::new("標的", 72.0),
            Col::new("Δ", 52.0),
            Col::new("IV", 56.0),
            Col::new("成本", 64.0),
            Col::new("原因", 220.0).left(),
        ];
        let w = table_width(&cols);
        let n = res.trades.len();
        v_flex()
            .child(table_header(&cols, cx).w(px(w)))
            .child(
                div().id("trades-scroll").h(px(260.0)).overflow_y_scroll().child(v_flex().w(px(w)).children(res.trades.iter().take(self.trades_shown).enumerate().map(
                    |(i, t)| {
                        let (act, c) = match t.action {
                            TradeAction::Open => ("開倉", p.green),
                            TradeAction::Roll => ("滾動", p.blue),
                            TradeAction::Rebalance => ("再平衡", p.yellow),
                            TradeAction::Close => ("平倉", p.red),
                        };
                        table_row(
                            SharedString::from(format!("tr-{i}")),
                            &cols,
                            vec![
                                Cell::new(t.date.to_string()),
                                Cell::new(act).color(c),
                                Cell::new(f2(t.strike)),
                                Cell::new(t.expiry.to_string()),
                                Cell::new(format!("{:+.2}", t.contracts)).color(sign_color(t.contracts)),
                                Cell::new(f2(t.price)),
                                Cell::new(f2(t.underlying)),
                                Cell::new(f2(t.delta)),
                                Cell::new(pct(t.iv)),
                                Cell::new(f0(t.cost)),
                                Cell::new(t.reason.clone()).color(p.muted),
                            ],
                            i % 2 == 1,
                            cx,
                        )
                    },
                ))),
            )
            .when(n > self.trades_shown, |d| {
                d.child(
                    h_flex().p_1().child(chip("more-trades", format!("顯示更多 (共 {n} 筆)"), false, cx).on_click(cx.listener(|this, _, _, cx| {
                        this.trades_shown += 500;
                        cx.notify();
                    }))),
                )
            })
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let (Some(res), Some(ch)) = (self.result.clone(), self.charts.as_ref()) else {
            return v_flex()
                .flex_1()
                .child(empty_state(
                    if self.running { "回測執行中…（背景執行緒，介面不會凍結）" } else { "設定參數後按「執行回測」。預設：QQQ · Δ0.80 · 540 天進場 · 剩 180 天滾動 · 1.5x 曝險" },
                    cx,
                ))
                .into_any_element();
        };
        let s = &res.stats;
        let bh = res.benchmark_stats.first().map(|(_, s)| s.clone()).unwrap_or_default();
        let lev = res.benchmark_stats.get(1).map(|(_, s)| s.clone()).unwrap_or_default();
        let years = s.years;
        let kpis = h_flex()
            .gap_2()
            .flex_wrap()
            .child(kpi("期末價值", money(s.final_value), Some(sign_color(s.total_return)), cx))
            .child(kpi("CAGR", pct2(s.cagr), Some(sign_color(s.cagr)), cx))
            .child(kpi("買進持有 CAGR", pct2(bh.cagr), Some(p.muted), cx))
            .child(kpi("槓桿ETF CAGR", pct2(lev.cagr), Some(p.orange), cx))
            .child(kpi("最大回撤", pct(s.max_drawdown), Some(p.red), cx))
            .child(kpi("Sharpe / Sortino", format!("{} / {}", f2(s.sharpe), f2(s.sortino)), None, cx))
            .child(kpi("平均曝險", format!("{:.2}x", res.avg_exposure), None, cx))
            .child(kpi("滾動次數 / 年", format!("{} / {:.1}", res.n_rolls, res.n_rolls as f64 / years.max(0.1)), None, cx));
        let extra = panel("持有成本分析 Carry", cx).flex_1().min_w(px(260.0)).child(
            panel_body()
                .child(kv("期間", format!("{} → {} ({:.1} 年)", s.start, s.end, years), cx))
                .child(kv("波動率指數", self.vol_name.clone(), cx))
                .child(kv("時間價值 / 年 (% 標的)", pct2(res.avg_extrinsic_pct_annual), cx))
                .child(kv("Carry / 年 (% 曝險)", pct2(res.avg_carry_pct_of_exposure), cx))
                .child(kv("Theta / 年 (% 權益)", pct2(res.avg_theta_pct_equity), cx))
                .child(kv("交易成本總額", money(res.total_costs), cx))
                .child(kv("成本拖累 / 年", pct2(res.cost_drag_annual), cx))
                .child(kv("平均無風險利率", pct2(res.avg_rate), cx))
                .child(kv("參數", res.config.label(), cx))
                .when(res.wiped_out, |d| {
                    d.child(badge(format!("⚠ 帳戶歸零於 {}", res.wiped_date.map(|d| d.to_string()).unwrap_or_default()), p.red))
                }),
        );
        let eq_spec = if self.log { ch.equity_log.clone() } else { ch.equity.clone() };
        v_flex()
            .gap_2()
            .child(kpis)
            .child(
                panel_with(
                    format!("{} 權益曲線 vs 基準", self.result_symbol),
                    chip("bt-log", "對數座標", self.log, cx).on_click(cx.listener(|this, _, _, cx| {
                        this.log = !this.log;
                        cx.notify();
                    })),
                    cx,
                )
                .h(px(340.0))
                .child(div().flex_1().min_h_0().p_1().child(xy_chart(eq_spec))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .h(px(210.0))
                    .child(panel("回撤 Drawdown", cx).flex_1().h_full().child(div().flex_1().min_h_0().p_1().child(xy_chart(ch.drawdown.clone()))))
                    .child(panel("曝險與 Delta", cx).flex_1().h_full().child(div().flex_1().min_h_0().p_1().child(xy_chart(ch.exposure.clone())))),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(panel("績效統計 (vs 基準)", cx).flex_none().child(self.stats_table(&res, cx)))
                    .child(extra),
            )
            .child(panel("年度報酬 (LEAPS vs 買進持有)", cx).h(px(240.0)).child(div().flex_1().min_h_0().p_1().child(xy_chart(ch.yearly.clone()))))
            .child(panel(format!("交易紀錄 ({} 筆)", res.trades.len()), cx).child(div().id("trades-h").overflow_x_scroll().child(self.trades_table(&res, cx))))
            .into_any_element()
    }
}

impl Render for BacktestPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let form = self.render_form(cx);
        let results = self.render_results(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(
                div()
                    .id("bt-form")
                    .w(px(400.0))
                    .flex_none()
                    .h_full()
                    .overflow_y_scroll()
                    .border_r_1()
                    .border_color(palette().border)
                    .child(form),
            )
            .child(div().id("bt-results").flex_1().min_w_0().h_full().overflow_y_scroll().p_2().child(results))
    }
}
