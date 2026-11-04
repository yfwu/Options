//! 策略實驗室 Strategy Lab: built-in templates → legs (from the loaded chain or a
//! synthetic Black-Scholes market) → payoff curves, Greeks, breakevens, PoP and a
//! Monte Carlo P&L distribution (background).

use std::rc::Rc;

use chrono::{Duration, NaiveDate};
use gpui::{div, prelude::*, px, Context, Entity, SharedString, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::InputState,
    v_flex, ActiveTheme, Sizable,
};
use optlab_core::montecarlo::{bootstrap_model, strategy_pnl_distribution, Distribution, PathModel};
use optlab_core::strategy::{build_from_chain, build_synthetic, LegInstrument, MarketParams, Strategy, StrategySummary, StrategyTemplate, TemplateParams, ALL_TEMPLATES};
use optlab_core::OptionKind;

use crate::data_bridge;
use crate::state::{AppState, StatusLevel};
use crate::theme::palette;
use crate::widgets::charts::{fmt_auto, fmt_money, fmt_pct, xy_chart, ChartSpec, RefLine, Series};
use crate::widgets::ui::*;

#[derive(Clone, Copy, PartialEq)]
enum McModel {
    Gbm,
    Merton,
    Heston,
    Bootstrap,
}

impl McModel {
    const ALL: [McModel; 4] = [McModel::Gbm, McModel::Merton, McModel::Heston, McModel::Bootstrap];
    fn label(self) -> &'static str {
        match self {
            McModel::Gbm => "GBM",
            McModel::Merton => "Merton 跳躍",
            McModel::Heston => "Heston",
            McModel::Bootstrap => "歷史拔靴",
        }
    }
}

struct Inputs {
    symbol: Entity<InputState>,
    spot: Entity<InputState>,
    iv: Entity<InputState>,
    r: Entity<InputState>,
    q: Entity<InputState>,
    long_delta: Entity<InputState>,
    short_delta: Entity<InputState>,
    long_dte: Entity<InputState>,
    short_dte: Entity<InputState>,
    width: Entity<InputState>,
    qty: Entity<InputState>,
    eval_days: Entity<InputState>,
    vol_shift: Entity<InputState>,
    drift: Entity<InputState>,
    paths: Entity<InputState>,
}

struct Built {
    strategy: Strategy,
    params: MarketParams,
    sigma: f64,
    drift: f64,
    summary: StrategySummary,
    eval_date: NaiveDate,
    from_chain: bool,
    payoff: Rc<ChartSpec>,
}

pub struct StrategyPage {
    state: Entity<AppState>,
    template: StrategyTemplate,
    use_chain: bool,
    model: McModel,
    inp: Inputs,
    errors: FormErrors,
    build_error: Option<String>,
    built: Option<Built>,
    mc: Option<(Distribution, Rc<ChartSpec>, String)>,
    mc_running: bool,
    mc_gen: u64,
}

impl StrategyPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let st = state.read(cx);
        let sym = st.settings.default_symbol.clone();
        let r = st.settings.risk_free;
        let spot = data_bridge::bundled_prices(&sym).ok().and_then(|p| p.bars.last().map(|b| b.close)).unwrap_or(500.0);
        let q = data_bridge::default_dividend_yield(&sym);
        let iv = data_bridge::default_vol_for(&sym).ok().and_then(|(_, v)| v.last()).map(|(_, v)| v * 0.92).unwrap_or(22.0);
        let template = StrategyTemplate::LeapsStockReplacement;
        let tp = template.default_params();
        let inp = Inputs {
            symbol: new_input(sym, window, cx),
            spot: new_input(format!("{spot:.2}"), window, cx),
            iv: new_input(format!("{iv:.1}"), window, cx),
            r: new_input(format!("{:.2}", r * 100.0), window, cx),
            q: new_input(format!("{:.2}", q * 100.0), window, cx),
            long_delta: new_input(format!("{}", tp.long_delta), window, cx),
            short_delta: new_input(format!("{}", tp.short_delta), window, cx),
            long_dte: new_input(tp.long_dte.to_string(), window, cx),
            short_dte: new_input(tp.short_dte.to_string(), window, cx),
            width: new_input(format!("{}", tp.width_pct * 100.0), window, cx),
            qty: new_input(format!("{}", tp.qty), window, cx),
            eval_days: new_input_ph("", "空白 = 到期前一半", window, cx),
            vol_shift: new_input("0", window, cx),
            drift: new_input_ph("", "空白 = 風險中性 r−q", window, cx),
            paths: new_input("20000", window, cx),
        };
        let mut this = Self {
            state,
            template,
            use_chain: false,
            model: McModel::Gbm,
            inp,
            errors: FormErrors::default(),
            build_error: None,
            built: None,
            mc: None,
            mc_running: false,
            mc_gen: 0,
        };
        this.rebuild(window, cx);
        this
    }

    pub fn on_chain_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(c) = self.state.read(cx).chain.clone() {
            self.use_chain = true;
            let sym = c.underlying.clone();
            let spot = c.spot;
            self.inp.symbol.update(cx, |s, cx| s.set_value(sym, window, cx));
            self.inp.spot.update(cx, |s, cx| s.set_value(format!("{spot:.2}"), window, cx));
            self.rebuild(window, cx);
        }
    }

    fn select_template(&mut self, t: StrategyTemplate, window: &mut Window, cx: &mut Context<Self>) {
        self.template = t;
        let tp = t.default_params();
        let set = |e: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| e.update(cx, |s, cx| s.set_value(v, window, cx));
        set(&self.inp.long_delta, format!("{}", tp.long_delta), window, cx);
        set(&self.inp.short_delta, format!("{}", tp.short_delta), window, cx);
        set(&self.inp.long_dte, tp.long_dte.to_string(), window, cx);
        set(&self.inp.short_dte, tp.short_dte.to_string(), window, cx);
        set(&self.inp.width, format!("{}", tp.width_pct * 100.0), window, cx);
        self.rebuild(window, cx);
    }

    /// Build the strategy and analytics from the form (synchronous; MC runs in background).
    pub fn rebuild(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let mut e = FormErrors::default();
        let i = &self.inp;
        let sym = input_text(&i.symbol, cx).to_ascii_uppercase();
        let spot = e.num_range("spot", &i.spot, 0.01, 1e7, cx);
        let iv = e.num_range("iv", &i.iv, 1.0, 300.0, cx) / 100.0;
        let r = e.num_range("r", &i.r, -5.0, 30.0, cx) / 100.0;
        let q = e.num_range("q", &i.q, 0.0, 30.0, cx) / 100.0;
        let tp = TemplateParams {
            long_delta: e.num_range("long_delta", &i.long_delta, 0.01, 0.99, cx),
            short_delta: e.num_range("short_delta", &i.short_delta, 0.01, 0.99, cx),
            long_dte: e.int("long_dte", &i.long_dte, 1, 1500, cx),
            short_dte: e.int("short_dte", &i.short_dte, 1, 1500, cx),
            width_pct: e.num_range("width", &i.width, -100.0, 100.0, cx) / 100.0,
            qty: e.num_range("qty", &i.qty, 0.01, 10000.0, cx),
            r,
            q,
        };
        let vol_shift = e.num_range("vol_shift", &i.vol_shift, -50.0, 100.0, cx) / 100.0;
        let drift_txt = input_text(&i.drift, cx);
        let drift = if drift_txt.is_empty() { r - q } else { e.num_range("drift", &i.drift, -100.0, 200.0, cx) / 100.0 };
        let eval_txt = input_text(&i.eval_days, cx);
        let eval_days = if eval_txt.is_empty() { None } else { Some(e.int("eval_days", &i.eval_days, 0, 2000, cx)) };
        let ok = e.is_empty();
        self.errors = e;
        if !ok {
            self.built = None;
            cx.notify();
            return;
        }
        let chain = if self.use_chain { self.state.read(cx).chain.clone() } else { None };
        let res: Result<(Strategy, f64, NaiveDate, bool), String> = match &chain {
            Some(c) => build_from_chain(self.template, c, &tp)
                .map(|s| (s, c.spot, c.as_of_date(), true))
                .map_err(|e| format!("無法由期權鏈建立：{e:#}")),
            None => {
                let today = chrono::Local::now().date_naive();
                let mut s = build_synthetic(self.template, spot, iv, r, q, today, &tp);
                s.underlying = sym.clone();
                Ok((s, spot, today, false))
            }
        };
        let (strategy, spot0, created, from_chain) = match res {
            Ok(v) => v,
            Err(err) => {
                self.build_error = Some(err.clone());
                self.built = None;
                self.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, err, cx));
                cx.notify();
                return;
            }
        };
        self.build_error = None;
        let params = MarketParams::new(spot0, r, q).with_vol_shift(vol_shift);
        // sigma for probability: average IV of option legs (vega-ish weighting by |qty|)
        let opt_legs: Vec<_> = strategy.legs.iter().filter(|l| l.is_option()).collect();
        let sigma = if opt_legs.is_empty() {
            iv
        } else {
            (opt_legs.iter().map(|l| l.iv * l.quantity.abs()).sum::<f64>() / opt_legs.iter().map(|l| l.quantity.abs()).sum::<f64>()) + vol_shift
        }
        .max(0.01);
        let exp = strategy.expiry_date();
        let total_days = (exp - created).num_days().max(1);
        let eval_date = match eval_days {
            Some(d) => (created + Duration::days(d)).min(exp),
            None => created + Duration::days(total_days / 2),
        };
        let summary = strategy.summary(exp, &params, sigma, drift);
        let payoff = Self::payoff_chart(&strategy, &params, created, eval_date, exp, &summary, sigma);
        self.built = Some(Built { strategy, params, sigma, drift, summary, eval_date, from_chain, payoff });
        self.mc = None;
        self.run_mc(cx);
        cx.notify();
    }

    fn payoff_chart(s: &Strategy, params: &MarketParams, created: NaiveDate, mid: NaiveDate, exp: NaiveDate, sum: &StrategySummary, sigma: f64) -> Rc<ChartSpec> {
        let p = palette();
        let spot = params.spot;
        let t = ((exp - created).num_days() as f64 / 365.0).max(1.0 / 365.0);
        let w = (2.5 * sigma * t.sqrt()).clamp(0.15, 0.9);
        let lo = spot * (1.0 - w).max(0.05);
        let hi = spot * (1.0 + w * 1.1);
        let euro = MarketParams { american: false, ..*params };
        let at_exp = s.pnl_curve(lo, hi, 241, exp, &euro);
        let today = s.pnl_curve(lo, hi, 241, created, &euro);
        let midc = s.pnl_curve(lo, hi, 241, mid, &euro);
        let mut spec = ChartSpec::new()
            .y_fmt(fmt_money)
            .x_fmt(fmt_auto)
            .series(Series::area(format!("到期 {exp}"), at_exp, p.blue, 0.0).width(1.8))
            .series(Series::line(format!("今日 {created}"), today, p.yellow).width(1.3))
            .series(Series::line(format!("中間日 {mid}"), midc, p.purple).width(1.2).dashed())
            .hline(RefLine::new(0.0, p.muted).solid())
            .vline(RefLine::new(spot, p.green).label(format!("現價 {spot:.2}")));
        for (i, b) in sum.breakevens.iter().enumerate() {
            spec = spec.vline(RefLine::new(*b, p.orange).label(if i == 0 { format!("損益兩平 {b:.2}") } else { format!("{b:.2}") }));
        }
        spec.into_rc()
    }

    fn mc_model(&self, cx: &mut Context<Self>, sigma: f64, drift: f64) -> Result<PathModel, String> {
        Ok(match self.model {
            McModel::Gbm => PathModel::Gbm { mu: drift, sigma },
            McModel::Merton => PathModel::MertonJump { mu: drift, sigma: (sigma * 0.85).max(0.01), lambda: 0.6, jump_mean: -0.05, jump_std: 0.07 },
            McModel::Heston => PathModel::Heston { mu: drift, v0: sigma * sigma, kappa: 2.0, theta: sigma * sigma, xi: 0.6, rho: -0.7 },
            McModel::Bootstrap => {
                let sym = self.built.as_ref().map(|b| b.strategy.underlying.clone()).unwrap_or_default();
                let series = self.state.update(cx, |st, _| st.prices(&sym))?;
                bootstrap_model(&series, 10)
            }
        })
    }

    fn run_mc(&mut self, cx: &mut Context<Self>) {
        let Some(b) = self.built.as_ref() else { return };
        let n = parse_num(&input_text(&self.inp.paths, cx)).unwrap_or(20000.0).clamp(500.0, 500_000.0) as usize;
        let (sigma, drift) = (b.sigma, b.drift);
        let model = match self.mc_model(cx, sigma, drift) {
            Ok(m) => m,
            Err(e) => {
                self.state.update(cx, |st, cx| st.set_status(StatusLevel::Warn, format!("Monte Carlo：{e}（歷史拔靴需內建資料的標的）"), cx));
                return;
            }
        };
        let Some(b) = self.built.as_ref() else { return };
        let strategy = b.strategy.clone();
        let params = b.params;
        let horizon = b.strategy.expiry_date();
        let spot = params.spot;
        self.mc_gen += 1;
        let gen = self.mc_gen;
        self.mc_running = true;
        let label = format!("{} · {} 路徑 · 期末 {horizon}", model.label_zh(), n);
        let job = cx.background_executor().spawn(async move { strategy_pnl_distribution(&strategy, &model, spot, horizon, &params, n, 42) });
        cx.spawn(async move |this, cx| {
            let d = job.await;
            let _ = this.update(cx, |this, cx| {
                if this.mc_gen != gen {
                    return;
                }
                this.mc_running = false;
                let p = palette();
                let bw = if d.histogram.len() > 1 { (d.histogram[1].0 - d.histogram[0].0).abs() } else { 1.0 };
                let colors = d.histogram.iter().map(|(x, _)| if *x >= 0.0 { p.green } else { p.red }).collect();
                let spec = ChartSpec::new()
                    .x_fmt(fmt_money)
                    .y_fmt(fmt_pct)
                    .series(Series::bars("機率", d.histogram.clone(), p.blue, bw).colors(colors))
                    .vline(RefLine::new(d.mean, p.yellow).label(format!("平均 {}", money(d.mean))))
                    .vline(RefLine::new(d.p05, p.red).label("P5"))
                    .vline(RefLine::new(d.p95, p.green).label("P95"))
                    .no_legend()
                    .into_rc();
                this.mc = Some((d, spec, label));
                cx.notify();
            });
        })
        .detach();
    }

    fn render_templates(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        v_flex().gap_0p5().p_1().children(ALL_TEMPLATES.iter().map(|t| {
            let t = *t;
            let active = t == self.template;
            let fit = t.bull_leverage_fit();
            v_flex()
                .id(SharedString::from(format!("tpl-{}", t.key())))
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .cursor_pointer()
                .when(active, |d| d.bg(p.blue.opacity(0.16)))
                .when(!active, |d| d.hover(|s| s.bg(p.elevated)))
                .on_click(cx.listener(move |this, _, window, cx| this.select_template(t, window, cx)))
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().text_size(px(12.5)).text_color(if active { p.text } else { p.muted }).child(t.name_zh()))
                        .child(div().text_size(px(10.0)).text_color(p.yellow).child("★".repeat(fit as usize))),
                )
                .child(h_flex().gap_1().child(div().text_size(px(10.5)).text_color(p.dim).child(t.name_en())).child(badge(t.outlook().label_zh(), p.cyan)))
        }))
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let e = &self.errors;
        let i = &self.inp;
        let row = || h_flex().gap_2().items_start();
        let f = |label: &str, key: &str, inp: &Entity<InputState>| field_err(label.to_string(), inp, e.get(key), cx).flex_1();
        let chain = self.state.read(cx).chain.clone();
        let p = palette();
        v_flex()
            .gap_2()
            .p_2()
            .child(
                h_flex()
                    .gap_1()
                    .child(chip("src-chain", chain.as_ref().map(|c| format!("使用期權鏈 {} ({})", c.underlying, c.source)).unwrap_or_else(|| "期權鏈 (未載入)".into()), self.use_chain, cx).on_click(cx.listener(|this, _, w, cx| {
                        if this.state.read(cx).chain.is_some() {
                            this.use_chain = true;
                            this.rebuild(w, cx);
                        } else {
                            this.state.update(cx, |st, cx| st.set_status(StatusLevel::Warn, "尚未載入期權鏈：請先到「期權鏈」頁取得或載入示範資料", cx));
                        }
                    })))
                    .child(chip("src-syn", "合成 Black-Scholes", !self.use_chain, cx).on_click(cx.listener(|this, _, w, cx| {
                        this.use_chain = false;
                        this.rebuild(w, cx);
                    }))),
            )
            .when(!self.use_chain, |d| {
                d.child(row().child(f("標的", "symbol", &i.symbol)).child(f("現價", "spot", &i.spot)).child(f("IV %", "iv", &i.iv)))
            })
            .child(row().child(f("r 無風險 %", "r", &i.r)).child(f("q 股息 %", "q", &i.q)).child(f("數量 (組)", "qty", &i.qty)))
            .child(section_title("範本參數", cx))
            .child(row().child(f("主買方 |Δ|", "long_delta", &i.long_delta)).child(f("主賣方 |Δ|", "short_delta", &i.short_delta)).child(f("價差寬度 %", "width", &i.width)))
            .child(row().child(f("(遠月) DTE", "long_dte", &i.long_dte)).child(f("近月 DTE", "short_dte", &i.short_dte)))
            .child(section_title("分析設定", cx))
            .child(row().child(f("中間評估日 (天後)", "eval_days", &i.eval_days)).child(f("IV 平移 (點)", "vol_shift", &i.vol_shift)).child(f("年化漂移 μ %", "drift", &i.drift)))
            .child(row().child(labeled(
                "Monte Carlo 模型",
                h_flex().gap_1().children(McModel::ALL.iter().map(|m| {
                    let m = *m;
                    chip(SharedString::from(format!("mc-{}", m.label())), m.label(), self.model == m, cx).on_click(cx.listener(move |this, _, _, cx| {
                        this.model = m;
                        this.run_mc(cx);
                        cx.notify();
                    }))
                })),
                cx,
            )).child(f("路徑數", "paths", &i.paths)))
            .child(
                h_flex()
                    .gap_2()
                    .child(Button::new("rebuild").primary().small().label("建立 / 重算 (Ctrl+Enter)").on_click(cx.listener(|this, _, w, cx| this.rebuild(w, cx))))
                    .child(Button::new("mc").small().label(if self.mc_running { "模擬中…" } else { "重新模擬" }).loading(self.mc_running).on_click(cx.listener(|this, _, _, cx| {
                        this.run_mc(cx);
                        cx.notify();
                    }))),
            )
            .when(!e.is_empty(), |d| d.child(div().text_size(px(11.0)).text_color(p.red).child(e.summary())))
            .when_some(self.build_error.clone(), |d, err| d.child(div().text_size(px(11.0)).text_color(p.red).child(err)))
            .child(panel(format!("{} · 說明", self.template.name_zh()), cx).child(div().p_2().text_size(px(12.0)).text_color(cx.theme().muted_foreground).child(self.template.description_zh())))
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some(b) = self.built.as_ref() else {
            return v_flex().child(empty_state("修正參數後按「建立 / 重算」", cx)).into_any_element();
        };
        let s = &b.summary;
        let g = s.greeks;
        let mpl = s.max_profit_loss;
        let created = b.strategy.created;
        // legs table
        let cols = vec![
            Col::new("類型", 56.0).left(),
            Col::new("數量", 56.0),
            Col::new("履約價", 72.0),
            Col::new("到期", 88.0),
            Col::new("DTE", 48.0),
            Col::new("成交價", 70.0),
            Col::new("IV", 56.0),
            Col::new("Δ (部位)", 70.0),
            Col::new("Θ $/日", 64.0),
            Col::new("成本 $", 84.0),
        ];
        let legs = b.strategy.legs.iter().enumerate().map(|(i, l)| {
            let lg = l.greeks(b.params.spot, created, &b.params);
            let (kind, k, exp) = match l.instrument {
                LegInstrument::Option { kind, strike, expiry } => (if kind == OptionKind::Call { "Call" } else { "Put" }, f2(strike), expiry.to_string()),
                LegInstrument::Stock => ("股票", "—".into(), "—".into()),
            };
            let dte = l.expiry().map(|e| (e - created).num_days().to_string()).unwrap_or_default();
            table_row(
                SharedString::from(format!("leg-{i}")),
                &cols,
                vec![
                    Cell::new(kind).color(if kind == "Put" { p.red } else { p.green }),
                    Cell::new(format!("{:+}", l.quantity)).color(sign_color(l.quantity)),
                    Cell::new(k),
                    Cell::new(exp),
                    Cell::new(dte),
                    Cell::new(f2(l.entry_price)),
                    Cell::new(if l.is_option() { pct(l.iv) } else { "—".into() }).color(p.purple),
                    Cell::new(f1(lg.delta)),
                    Cell::new(f2(lg.theta)),
                    Cell::new(money(l.cost())),
                ],
                i % 2 == 1,
                cx,
            )
        });
        let maxp = if mpl.profit_unbounded { "無上限".to_string() } else { money(mpl.max_profit) };
        let maxl = if mpl.loss_unbounded { "無上限".to_string() } else { money(mpl.max_loss) };
        let be = if s.breakevens.is_empty() { "—".into() } else { s.breakevens.iter().map(|b| format!("{b:.2}")).collect::<Vec<_>>().join(" / ") };
        let kpis = h_flex()
            .gap_2()
            .flex_wrap()
            .child(kpi("進場成本 (淨)", money(s.entry_cost), Some(if s.entry_cost > 0.0 { p.red } else { p.green }), cx))
            .child(kpi("所需資金", money(s.capital_required), None, cx))
            .child(kpi("最大獲利", maxp, Some(p.green), cx))
            .child(kpi("最大損失", maxl, Some(p.red), cx))
            .child(kpi("損益兩平", be, Some(p.orange), cx))
            .child(kpi("獲利機率 PoP", pct(s.probability_of_profit), None, cx))
            .child(kpi("期望損益", money(s.expected_pnl), Some(sign_color(s.expected_pnl)), cx))
            .child(kpi("槓桿 (Δ$/資金)", format!("{:.2}x", s.leverage), Some(p.blue), cx));
        let greeks = h_flex()
            .gap_2()
            .flex_wrap()
            .child(kpi("Delta (股當量)", f1(g.delta), None, cx))
            .child(kpi("Gamma", f3(g.gamma), None, cx))
            .child(kpi("Theta $/日", f2(g.theta), Some(sign_color(g.theta)), cx))
            .child(kpi("Vega $/點", f2(g.vega), None, cx))
            .child(kpi("Rho $/點", f2(g.rho), None, cx))
            .child(kpi("Δ 名目 $", money(g.delta * b.params.spot), None, cx));
        let mc = match &self.mc {
            Some((d, spec, label)) => h_flex()
                .gap_2()
                .items_start()
                .child(panel(format!("Monte Carlo 損益分佈 · {label}"), cx).flex_1().h(px(260.0)).child(div().flex_1().min_h_0().p_1().child(xy_chart(spec.clone()))))
                .child(
                    panel("分佈統計", cx).w(px(230.0)).flex_none().child(
                        panel_body()
                            .child(kv("獲利機率", pct(d.prob_profit), cx))
                            .child(kv_colored("平均", money(d.mean), sign_color(d.mean), cx))
                            .child(kv("中位數", money(d.median), cx))
                            .child(kv("標準差", money(d.std), cx))
                            .child(kv("P5 / P95", format!("{} / {}", money(d.p05), money(d.p95)), cx))
                            .child(kv("P25 / P75", format!("{} / {}", money(d.p25), money(d.p75)), cx))
                            .child(kv_colored("VaR95", money(d.var95), p.red, cx))
                            .child(kv_colored("CVaR95", money(d.cvar95), p.red, cx))
                            .child(kv("最小 / 最大", format!("{} / {}", money(d.min), money(d.max)), cx)),
                    ),
                )
                .into_any_element(),
            None => panel("Monte Carlo 損益分佈", cx).h(px(80.0)).child(empty_state(if self.mc_running { "背景模擬中…" } else { "—" }, cx)).into_any_element(),
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_size(px(14.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(format!("{} · {}", b.strategy.underlying, b.strategy.name)))
                    .child(badge(if b.from_chain { "期權鏈實際報價 (mid)" } else { "合成 BS 定價" }, if b.from_chain { p.green } else { p.yellow }))
                    .child(muted_text(format!("建立日 {created} · σ(PoP) {} · μ {}", pct(b.sigma), pct(b.drift)), cx)),
            )
            .child(kpis)
            .child(greeks)
            .child(panel("部位明細 Legs", cx).child(v_flex().child(table_header(&cols, cx)).children(legs)))
            .child(
                panel(format!("損益圖 Payoff · 到期 / 今日 / 中間日 {}", b.eval_date), cx)
                    .h(px(330.0))
                    .child(div().flex_1().min_h_0().p_1().child(xy_chart(b.payoff.clone()))),
            )
            .child(mc)
            .into_any_element()
    }
}

impl Render for StrategyPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let templates = self.render_templates(cx);
        let form = self.render_form(cx);
        let results = self.render_results(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(div().id("tpl-list").w(px(210.0)).flex_none().h_full().overflow_y_scroll().border_r_1().border_color(p.border).child(templates))
            .child(div().id("lab-form").w(px(370.0)).flex_none().h_full().overflow_y_scroll().border_r_1().border_color(p.border).child(form))
            .child(div().id("lab-results").flex_1().min_w_0().h_full().overflow_y_scroll().p_2().child(results))
    }
}
