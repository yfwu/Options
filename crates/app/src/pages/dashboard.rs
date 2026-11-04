//! 市場總覽 Dashboard: price + 200-DMA, vol index, KPIs and advisor cards.

use std::rc::Rc;

use gpui::{div, prelude::*, px, Context, Entity, SharedString, Subscription, Window};
use gpui_component::{
    h_flex,
    select::{Select, SelectEvent, SelectState},
    v_flex, IndexPath, Sizable,
};
use optlab_core::advisor::{advise, Advice, MarketSnapshot};
use optlab_core::PriceSeries;

use crate::data_bridge;
use crate::state::{AppState, Page, StatusLevel};
use crate::theme::palette;
use crate::widgets::charts::{date_to_x, fmt_auto, xy_chart, ChartSpec, RefLine, Series};
use crate::widgets::ui::*;

const RANGES: [(&str, Option<f64>); 5] = [("1Y", Some(1.0)), ("3Y", Some(3.0)), ("5Y", Some(5.0)), ("10Y", Some(10.0)), ("全部", None)];

pub struct DashboardPage {
    state: Entity<AppState>,
    symbol: Entity<SelectState<Vec<SharedString>>>,
    range: usize,
    log: bool,
    data: Option<DashData>,
    error: Option<String>,
    _subs: Vec<Subscription>,
}

struct DashData {
    symbol: String,
    vol_name: String,
    snap: MarketSnapshot,
    last_close: f64,
    day_change: f64,
    advice: Vec<Advice>,
    prices: PriceSeries,
    vol: Vec<(chrono::NaiveDate, f64)>,
}

impl DashboardPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let syms: Vec<SharedString> = data_bridge::bundled_symbols().into_iter().map(SharedString::from).collect();
        let default = state.read(cx).settings.default_symbol.to_ascii_uppercase();
        let idx = syms.iter().position(|s| s.as_ref() == default).unwrap_or(0);
        let symbol = cx.new(|cx| SelectState::new(syms, Some(IndexPath::default().row(idx)), window, cx));
        let subs = vec![cx.subscribe_in(&symbol, window, |this, _, ev: &SelectEvent<Vec<SharedString>>, window, cx| {
            let SelectEvent::Confirm(_) = ev;
            this.reload(window, cx);
        })];
        let mut this = Self { state, symbol, range: 2, log: false, data: None, error: None, _subs: subs };
        this.load(cx);
        this
    }

    fn current_symbol(&self, cx: &Context<Self>) -> String {
        self.symbol.read(cx).selected_value().map(|s| s.to_string()).unwrap_or_else(|| "QQQ".into())
    }

    pub fn reload(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.load(cx);
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let sym = self.current_symbol(cx);
        let res = self.state.update(cx, |st, _| -> Result<DashData, String> {
            let prices = st.prices(&sym)?;
            let (vol_name, vol) = data_bridge::default_vol_for(&sym)?;
            let snap = MarketSnapshot::from_history(&prices, &vol);
            let advice = advise(&snap, None, None);
            let n = prices.bars.len();
            let last_close = prices.bars.last().map(|b| b.close).unwrap_or(f64::NAN);
            let day_change = if n >= 2 { prices.bars[n - 1].close / prices.bars[n - 2].close - 1.0 } else { f64::NAN };
            Ok(DashData {
                symbol: sym.clone(),
                vol_name,
                snap,
                last_close,
                day_change,
                advice,
                prices: (*prices).clone(),
                vol: vol.points.clone(),
            })
        });
        match res {
            Ok(d) => {
                self.data = Some(d);
                self.error = None;
            }
            Err(e) => {
                self.error = Some(e.clone());
                self.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, e, cx));
            }
        }
    }

    fn charts(&self) -> Option<(Rc<ChartSpec>, Rc<ChartSpec>)> {
        let d = self.data.as_ref()?;
        let p = palette();
        let last = d.prices.last_date()?;
        let start = RANGES[self.range].1.map(|y| last - chrono::Duration::days((y * 365.25) as i64));
        let bars = &d.prices.bars;
        // SMA on full history, then slice
        let closes: Vec<f64> = bars.iter().map(|b| b.adj_close).collect();
        let mut sma200 = vec![];
        let mut sma50 = vec![];
        let mut sum200 = 0.0;
        let mut sum50 = 0.0;
        for (i, c) in closes.iter().enumerate() {
            sum200 += c;
            sum50 += c;
            if i >= 200 {
                sum200 -= closes[i - 200];
            }
            if i >= 50 {
                sum50 -= closes[i - 50];
            }
            let x = date_to_x(bars[i].date);
            if start.is_some_and(|s| bars[i].date < s) {
                continue;
            }
            if i >= 199 {
                sma200.push((x, sum200 / 200.0));
            }
            if i >= 49 {
                sma50.push((x, sum50 / 50.0));
            }
        }
        let price: Vec<(f64, f64)> = bars
            .iter()
            .filter(|b| start.is_none_or(|s| b.date >= s))
            .map(|b| (date_to_x(b.date), b.adj_close))
            .collect();
        let price_spec = ChartSpec::new()
            .dates()
            .log_y(self.log)
            .y_fmt(fmt_auto)
            .series(Series::line(format!("{} 還原收盤", d.symbol), price, p.blue).width(1.4))
            .series(Series::line("50 日均線", sma50, p.yellow).width(1.0))
            .series(Series::line("200 日均線", sma200, p.orange).width(1.3))
            .into_rc();

        // vol index + realised vol (in vol points)
        let vol: Vec<(f64, f64)> = d
            .vol
            .iter()
            .filter(|(dd, _)| start.is_none_or(|s| *dd >= s) && *dd <= last)
            .map(|(dd, v)| (date_to_x(*dd), *v))
            .collect();
        let lr = d.prices.log_returns();
        let mut rv = vec![];
        for i in 20..=lr.len() {
            let date = bars[i].date;
            if start.is_some_and(|s| date < s) {
                continue;
            }
            let w = &lr[i - 20..i];
            let m = w.iter().sum::<f64>() / 20.0;
            let v = w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / 19.0;
            rv.push((date_to_x(date), (v * 252.0).sqrt() * 100.0));
        }
        let mean = d.snap.vol_mean_1y;
        let mut vol_spec = ChartSpec::new()
            .dates()
            .series(Series::area(d.vol_name.clone(), vol, p.purple, 0.0).width(1.2))
            .series(Series::line("RV20 實現波動", rv, p.cyan).width(1.0));
        if mean.is_finite() {
            vol_spec = vol_spec.hline(RefLine::new(mean, p.dim).label(format!("1年均值 {mean:.1}")));
        }
        Some((price_spec, vol_spec.into_rc()))
    }
}

impl Render for DashboardPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let toolbar = h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .flex_none()
            .child(div().w(px(130.0)).child(Select::new(&self.symbol).small()))
            .child(h_flex().gap_1().children(RANGES.iter().enumerate().map(|(i, (l, _))| {
                chip(SharedString::from(format!("rng-{i}")), *l, self.range == i, cx).on_click(cx.listener(move |this, _, _, cx| {
                    this.range = i;
                    cx.notify();
                }))
            })))
            .child(chip("log", "對數座標", self.log, cx).on_click(cx.listener(|this, _, _, cx| {
                this.log = !this.log;
                cx.notify();
            })))
            .child(div().flex_1())
            .child(chip("go-bt", "→ 以此標的回測", false, cx).on_click(cx.listener(|this, _, window, cx| {
                let sym = this.current_symbol(cx);
                let patch = crate::pages::backtest::BacktestParamsPatch { symbol: Some(sym), ..Default::default() };
                this.state.update(cx, |_, cx| cx.emit(crate::state::AppEvent::ApplyBacktestParams(Box::new(patch))));
                let _ = window;
            })))
            .child(chip("go-chain", "→ 期權鏈", false, cx).on_click(cx.listener(|this, _, _, cx| {
                this.state.update(cx, |st, cx| st.navigate(Page::Chain, cx));
            })));

        let Some(d) = self.data.as_ref() else {
            return v_flex().size_full().child(toolbar).child(empty_state(self.error.clone().unwrap_or_else(|| "載入中…".into()), cx)).into_any_element();
        };
        let s = &d.snap;
        let regime = if !s.sma200.is_finite() {
            ("資料不足", p.muted)
        } else if s.above_sma200 && s.vol_now < s.vol_mean_1y.max(18.0) {
            ("多頭 · 低波動", p.green)
        } else if s.above_sma200 {
            ("多頭 · 波動偏高", p.yellow)
        } else if s.drawdown_from_high > -0.2 {
            ("跌破 200DMA · 修正", p.orange)
        } else {
            ("空頭 · 高風險", p.red)
        };
        let kpis = h_flex()
            .gap_2()
            .flex_wrap()
            .child(kpi(format!("{} 收盤 ({})", d.symbol, s.date), f2(d.last_close), None, cx))
            .child(kpi("日漲跌", pct2(d.day_change), Some(sign_color(d.day_change)), cx))
            .child(kpi("距 200DMA", pct(s.spot / s.sma200 - 1.0), Some(sign_color(s.spot / s.sma200 - 1.0)), cx))
            .child(kpi("距歷史高點", pct(s.drawdown_from_high), Some(if s.drawdown_from_high < -0.1 { p.red } else { p.text }), cx))
            .child(kpi(format!("{} 波動指數", d.vol_name), f1(s.vol_now), Some(p.purple), cx))
            .child(kpi("IV Rank / 百分位 (1Y)", format!("{} / {}", pct(s.iv_rank_1y), pct(s.iv_percentile_1y)), None, cx))
            .child(kpi("RV20 / RV60", format!("{} / {}", pct(s.rv20), pct(s.rv60)), Some(p.cyan), cx))
            .child(kpi("12M 報酬", pct(s.ret_12m), Some(sign_color(s.ret_12m)), cx))
            .child(kpi("市場狀態 Regime", regime.0, Some(regime.1), cx));

        let (price_spec, vol_spec) = self.charts().unzip();
        let left = v_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .child(
                panel(format!("{} 價格與均線 (還原權值)  ·  趨勢分數 {:+.2}", d.symbol, s.trend_score), cx)
                    .h(px(370.0))
                    .child(div().flex_1().min_h_0().p_1().children(price_spec.map(xy_chart))),
            )
            .child(
                panel(format!("{} 波動率指數 vs 實現波動 (vol 點)  ·  VRP {:+.1} 點", d.vol_name, s.vrp * 100.0), cx)
                    .h(px(250.0))
                    .child(div().flex_1().min_h_0().p_1().children(vol_spec.map(xy_chart))),
            );
        let right = v_flex()
            .w(px(400.0))
            .flex_none()
            .gap_2()
            .child(
                panel("建議 Advisor · 依目前市況", cx).child(
                    v_flex().p_2().gap_2().children(d.advice.iter().map(|a| advice_card(a, cx))),
                ),
            )
            .child(
                panel("技術指標", cx).child(
                    panel_body()
                        .child(kv("50 日均線", f2(s.sma50), cx))
                        .child(kv("200 日均線", f2(s.sma200), cx))
                        .child(kv_colored("200DMA 斜率 (20日)", pct2(s.sma200_slope), sign_color(s.sma200_slope), cx))
                        .child(kv("距 52 週高點", pct(s.drawdown_from_52w_high), cx))
                        .child(kv_colored("1M / 3M 報酬", format!("{} / {}", pct(s.ret_1m), pct(s.ret_3m)), sign_color(s.ret_3m), cx))
                        .child(kv_colored("6M 報酬", pct(s.ret_6m), sign_color(s.ret_6m), cx))
                        .child(kv("波動指數 1 年均值", f1(s.vol_mean_1y), cx))
                        .child(muted_text("資料：內建離線歷史 (Yahoo/CBOE 鏡像)。價格為還原權值收盤，可在設定頁改用即時來源。", cx)),
                ),
            );
        v_flex()
            .size_full()
            .child(toolbar)
            .child(
                div()
                    .id("dash-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(v_flex().px_3().pb_3().gap_2().child(kpis).child(h_flex().items_start().gap_2().child(left).child(right))),
            )
            .into_any_element()
    }
}
