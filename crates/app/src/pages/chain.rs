//! 期權鏈 Option Chain: provider / snapshot / CSV / demo loading in the background,
//! T-shaped chain table, analytics (expected move, skew, P/C, max pain, GEX) and charts.

use std::rc::Rc;
use std::sync::Arc;

use chrono::NaiveDate;
use gpui::{div, prelude::*, px, Context, Entity, ScrollHandle, SharedString, Subscription, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectState},
    v_flex, IndexPath, Sizable,
};
use optlab_core::surface::{gamma_exposure, max_pain, put_call_ratio, smile_points, ChainAnalytics, GexProfile, MaxPain, PutCallRatio, SmilePoint};
use optlab_core::{OptionChain, OptionKind, OptionQuote};

use crate::data_bridge::{self, ChainSource};
use crate::state::{AppState, Page, StatusLevel};
use crate::theme::{palette, MONO_FONT};
use crate::widgets::charts::{fmt_auto, fmt_pct, xy_chart, ChartSpec, RefLine, Series};
use crate::widgets::ui::*;

const RANGES: [(&str, Option<f64>); 5] = [("±5%", Some(0.05)), ("±10%", Some(0.10)), ("±20%", Some(0.20)), ("±40%", Some(0.40)), ("全部", None)];

pub struct ChainPage {
    state: Entity<AppState>,
    source: Entity<SelectState<Vec<SharedString>>>,
    symbol: Entity<InputState>,
    csv_path: Entity<InputState>,
    loading: bool,
    error: Option<String>,
    chain: Option<Arc<OptionChain>>,
    label: String,
    analytics: Option<Arc<ChainAnalytics>>,
    expiry: Option<NaiveDate>,
    range: usize,
    per_exp: Option<PerExpiry>,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

struct PerExpiry {
    smile: Vec<SmilePoint>,
    gex: GexProfile,
    max_pain: Option<MaxPain>,
    pcr: PutCallRatio,
    smile_chart: Rc<ChartSpec>,
    gex_chart: Rc<ChartSpec>,
}

impl ChainPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let labels: Vec<SharedString> = ChainSource::ALL.iter().map(|s| SharedString::from(s.label())).collect();
        let source = cx.new(|cx| SelectState::new(labels, Some(IndexPath::default()), window, cx));
        let sym = state.read(cx).settings.default_symbol.clone();
        let symbol = new_input_ph(sym, "代號 e.g. QQQ / SPY / SPX", window, cx);
        let csv_path = new_input_ph("", "CSV 檔案完整路徑 (長格式：expiry,strike,type,bid,ask,iv…)", window, cx);
        let subs = vec![
            cx.subscribe_in(&symbol, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.fetch(window, cx);
                }
            }),
            cx.subscribe_in(&source, window, |_, _, _: &SelectEvent<Vec<SharedString>>, _, cx| cx.notify()),
        ];
        Self {
            state,
            source,
            symbol,
            csv_path,
            loading: false,
            error: None,
            chain: None,
            label: String::new(),
            analytics: None,
            expiry: None,
            range: 2,
            per_exp: None,
            scroll: ScrollHandle::new(),
            _subs: subs,
        }
    }

    fn source_value(&self, cx: &Context<Self>) -> ChainSource {
        let idx = self.source.read(cx).selected_index(cx).map(|i| i.row).unwrap_or(0);
        ChainSource::ALL.get(idx).copied().unwrap_or(ChainSource::Auto)
    }

    fn rq(&self, sym: &str, cx: &Context<Self>) -> (f64, f64) {
        let st = self.state.read(cx);
        let q = match data_bridge::default_dividend_yield(sym) {
            0.0 => st.settings.dividend_yield,
            v => v,
        };
        (st.settings.risk_free, q)
    }

    /// Load the synthetic demo chain (used by OPTLAB_AUTORUN and the empty-state button).
    pub fn load_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idx = ChainSource::ALL.iter().position(|s| *s == ChainSource::Demo).unwrap_or(0);
        self.source.update(cx, |s, cx| s.set_selected_index(Some(IndexPath::default().row(idx)), window, cx));
        self.fetch(window, cx);
    }

    pub fn fetch(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        let source = self.source_value(cx);
        let sym = input_text(&self.symbol, cx).to_ascii_uppercase();
        if sym.is_empty() && !matches!(source, ChainSource::Csv | ChainSource::CboeSample) {
            self.error = Some("請輸入標的代號".into());
            cx.notify();
            return;
        }
        let path = input_text(&self.csv_path, cx);
        let svc = self.state.read(cx).svc.clone();
        self.loading = true;
        self.error = None;
        let task = self.state.update(cx, |st, cx| {
            st.set_status(StatusLevel::Info, format!("取得 {sym} 期權鏈：{} …", source.label()), cx);
            st.begin_task(format!("期權鏈 {sym}"), cx)
        });
        let (r, q) = self.rq(&sym, cx);
        let job = cx.background_executor().spawn(async move {
            let (chain, label) = data_bridge::load_chain(&svc, source, &sym, &path)?;
            if chain.quotes.is_empty() {
                return Err("期權鏈沒有任何報價".to_string());
            }
            let analytics = ChainAnalytics::compute(&chain, r, q);
            Ok((chain, label, analytics))
        });
        cx.spawn(async move |this, cx| {
            let res = job.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                this.state.update(cx, |st, cx| st.end_task(task, cx));
                match res {
                    Ok((chain, label, analytics)) => {
                        let msg = format!(
                            "已載入 {} 期權鏈 · {} 檔 · {} 個到期日 · 來源 {label}",
                            chain.underlying,
                            chain.quotes.len(),
                            chain.expiries().len()
                        );
                        let chain = Arc::new(chain);
                        this.expiry = chain.expiry_near_dte(30).or_else(|| chain.expiries().first().copied());
                        this.chain = Some(chain.clone());
                        this.label = label.clone();
                        this.analytics = Some(Arc::new(analytics));
                        this.recompute_expiry(cx);
                        this.state.update(cx, |st, cx| {
                            st.set_chain((*chain).clone(), label, cx);
                            st.set_status(StatusLevel::Ok, msg, cx);
                        });
                        this.scroll_to_atm();
                    }
                    Err(e) => {
                        let short: String = e.lines().next().unwrap_or("").chars().take(160).collect();
                        this.error = Some(e.clone());
                        this.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, format!("期權鏈載入失敗：{short}"), cx));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn visible_strikes(&self) -> Vec<f64> {
        let (Some(c), Some(e)) = (self.chain.as_ref(), self.expiry) else { return vec![] };
        let strikes = c.strikes(e);
        match RANGES[self.range].1 {
            Some(w) => strikes.into_iter().filter(|k| (k / c.spot - 1.0).abs() <= w).collect(),
            None => strikes,
        }
    }

    fn scroll_to_atm(&self) {
        let Some(c) = self.chain.as_ref() else { return };
        let ks = self.visible_strikes();
        if let Some(i) = ks.iter().position(|k| *k >= c.spot) {
            self.scroll.scroll_to_item((i + 10).min(ks.len().saturating_sub(1)));
        }
    }

    fn recompute_expiry(&mut self, cx: &Context<Self>) {
        let (Some(c), Some(e)) = (self.chain.clone(), self.expiry) else {
            self.per_exp = None;
            return;
        };
        let (r, q) = self.rq(&c.underlying, cx);
        let smile = smile_points(&c, e, r, q);
        let gex = gamma_exposure(&c, r, q, Some(e));
        let mp = max_pain(&c, e);
        let pcr = put_call_ratio(&c, Some(e));
        let p = palette();
        let call: Vec<(f64, f64)> = smile.iter().filter_map(|s| s.call_iv.map(|v| (s.strike, v))).collect();
        let put: Vec<(f64, f64)> = smile.iter().filter_map(|s| s.put_iv.map(|v| (s.strike, v))).collect();
        let fit: Vec<(f64, f64)> = smile.iter().filter_map(|s| s.fitted.map(|v| (s.strike, v))).collect();
        let mut smile_chart = ChartSpec::new()
            .y_fmt(fmt_pct)
            .x_fmt(fmt_auto)
            .series(Series::points("Call IV", call, p.green))
            .series(Series::points("Put IV", put, p.red))
            .series(Series::line("擬合微笑", fit, p.blue).width(1.5))
            .vline(RefLine::new(c.spot, p.yellow).label("現價"));
        if let Some(mp) = &mp {
            smile_chart = smile_chart.vline(RefLine::new(mp.strike, p.purple).label("Max Pain"));
        }
        let w = RANGES[self.range].1.unwrap_or(0.5).max(0.1);
        let bars: Vec<(f64, f64)> = gex
            .by_strike
            .iter()
            .filter(|g| (g.strike / c.spot - 1.0).abs() <= w)
            .map(|g| (g.strike, g.net_gex / 1e6))
            .collect();
        let bw = {
            let ks: Vec<f64> = bars.iter().map(|b| b.0).collect();
            ks.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min).min(c.spot * 0.02).max(0.01) * 0.8
        };
        let mut gex_chart = ChartSpec::new()
            .x_fmt(fmt_auto)
            .y_fmt(|v| format!("{v:.1}M"))
            .series(Series::bars("Net GEX ($M / 1%)", bars, p.blue, bw).signed_colors())
            .vline(RefLine::new(c.spot, p.yellow).label("現價"));
        if let Some(f) = gex.gamma_flip {
            gex_chart = gex_chart.vline(RefLine::new(f, p.orange).label("Gamma flip"));
        }
        self.per_exp = Some(PerExpiry { smile, gex, max_pain: mp, pcr, smile_chart: smile_chart.into_rc(), gex_chart: gex_chart.into_rc() });
    }

    fn select_expiry(&mut self, e: NaiveDate, cx: &mut Context<Self>) {
        self.expiry = Some(e);
        self.recompute_expiry(cx);
        self.scroll_to_atm();
        cx.notify();
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let src = self.source_value(cx);
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .flex_none()
            .border_b_1()
            .border_color(palette().border)
            .child(div().w(px(230.0)).child(Select::new(&self.source).small()))
            .child(div().w(px(110.0)).child(Input::new(&self.symbol).small()))
            .when(src == ChainSource::Csv, |d| d.child(div().w(px(380.0)).child(Input::new(&self.csv_path).small())))
            .child(
                Button::new("fetch-chain")
                    .primary()
                    .small()
                    .label(if self.loading { "取得中…" } else { "取得" })
                    .loading(self.loading)
                    .on_click(cx.listener(|this, _, window, cx| this.fetch(window, cx))),
            )
            .child(Button::new("demo-chain").small().label("載入合成示範").on_click(cx.listener(|this, _, window, cx| this.load_demo(window, cx))))
            .child(div().flex_1())
            .child(h_flex().gap_1().children(RANGES.iter().enumerate().map(|(i, (l, _))| {
                chip(SharedString::from(format!("cr-{i}")), *l, self.range == i, cx).on_click(cx.listener(move |this, _, _, cx| {
                    this.range = i;
                    this.recompute_expiry(cx);
                    this.scroll_to_atm();
                    cx.notify();
                }))
            })))
            .when(self.chain.is_some(), |d| {
                d.child(chip("save-snap", "存成快照", false, cx).on_click(cx.listener(|this, _, _, cx| this.save_snapshot(cx))))
                    .child(chip("export-csv", "匯出 CSV", false, cx).on_click(cx.listener(|this, _, window, cx| this.export_csv(window, cx))))
                    .child(chip("to-lab", "→ 策略實驗室", false, cx).on_click(cx.listener(|this, _, _, cx| {
                        this.state.update(cx, |st, cx| st.navigate(Page::Strategy, cx));
                    })))
            })
    }

    fn save_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.chain.clone() else { return };
        let svc = self.state.read(cx).svc.clone();
        let res = svc.store().save_chain(&c);
        self.state.update(cx, |st, cx| match res {
            Ok(p) => st.set_status(StatusLevel::Ok, format!("已儲存快照：{}", p.display()), cx),
            Err(e) => st.set_status(StatusLevel::Error, format!("儲存快照失敗：{e:#}"), cx),
        });
    }

    fn export_csv(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(c) = self.chain.clone() else { return };
        let svc = self.state.read(cx).svc.clone();
        let dir = svc.store().root().join("exports");
        let path = dir.join(format!("{}_{}_{}.csv", c.underlying, c.as_of.format("%Y%m%d_%H%M"), c.source));
        let mut out = String::from("contract,underlying,underlying_price,expiry,strike,type,bid,ask,last,volume,open_interest,iv,delta,gamma,theta,vega\n");
        let o = |v: Option<f64>| v.map(|x| format!("{x:.6}")).unwrap_or_default();
        for q in &c.quotes {
            out.push_str(&format!(
                "{},{},{:.4},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                q.contract,
                c.underlying,
                c.spot,
                q.expiry,
                q.strike,
                if q.kind == OptionKind::Call { "call" } else { "put" },
                q.bid,
                q.ask,
                q.last,
                q.volume,
                q.open_interest,
                o(q.iv),
                o(q.delta),
                o(q.gamma),
                o(q.theta),
                o(q.vega)
            ));
        }
        let res = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, out));
        match res {
            Ok(()) => {
                let ps = path.display().to_string();
                self.csv_path.update(cx, |s, cx| s.set_value(ps.clone(), window, cx));
                self.state.update(cx, |st, cx| st.set_status(StatusLevel::Ok, format!("已匯出 CSV：{ps}（可用「CSV 檔案路徑」重新匯入）"), cx));
            }
            Err(e) => self.state.update(cx, |st, cx| st.set_status(StatusLevel::Error, format!("匯出失敗：{e}"), cx)),
        }
    }

    fn render_expiries(&self, c: &OptionChain, cx: &mut Context<Self>) -> impl IntoElement {
        let d0 = c.as_of_date();
        let p = palette();
        div().id("exp-scroll").flex_none().overflow_x_scroll().px_3().py_1().child(h_flex().gap_1().children(c.expiries().into_iter().filter(|e| *e >= d0).map(|e| {
            let dte = (e - d0).num_days();
            let active = self.expiry == Some(e);
            let label = format!("{} · {}d", e.format("%y-%m-%d"), dte);
            let mut ch = chip(SharedString::from(format!("exp-{e}")), label, active, cx);
            if dte >= 365 && !active {
                ch = ch.text_color(p.yellow);
            }
            ch.on_click(cx.listener(move |this, _, _, cx| this.select_expiry(e, cx)))
        })))
    }

    fn render_table(&self, c: &OptionChain, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some(e) = self.expiry else { return div().into_any_element() };
        let strikes = self.visible_strikes();
        let atm = c.atm_strike(e);
        let cw = 50.0;
        let side = ["Bid", "Ask", "IV", "Δ", "OI", "Vol"];
        let mut cols: Vec<Col> = side.iter().map(|t| Col::new(*t, cw)).collect();
        cols.push(Col::new("履約價", 78.0).center());
        cols.extend(side.iter().rev().map(|t| Col::new(*t, cw)));
        let total = table_width(&cols);
        let cells_for = |q: Option<&OptionQuote>, itm: bool, rev: bool| -> Vec<Cell> {
            let bg = if itm { Some(p.blue.opacity(0.16)) } else { None };
            let mut v = match q {
                Some(q) => vec![
                    Cell::new(f2(q.bid)),
                    Cell::new(f2(q.ask)),
                    Cell::new(q.iv.map(|v| format!("{:.1}", v * 100.0)).unwrap_or_else(|| "—".into())).color(p.purple),
                    Cell::new(q.delta.map(|d| format!("{d:.2}")).unwrap_or_else(|| "—".into())),
                    Cell::new(f0(q.open_interest)).color(p.muted),
                    Cell::new(f0(q.volume)).color(p.dim),
                ],
                None => (0..6).map(|_| Cell::new("")).collect(),
            };
            if let Some(b) = bg {
                v = v.into_iter().map(|c| c.bg(b)).collect();
            }
            if rev {
                v.reverse();
            }
            v
        };
        let header = h_flex()
            .w(px(total))
            .h(px(20.0))
            .text_size(px(11.0))
            .child(div().w(px(cw * 6.0)).flex().justify_center().text_color(p.green).child("CALLS 買權"))
            .child(div().w(px(78.0)))
            .child(div().w(px(cw * 6.0)).flex().justify_center().text_color(p.red).child("PUTS 賣權"));
        let rows = strikes.iter().enumerate().map(|(i, k)| {
            let call = c.find(e, *k, OptionKind::Call);
            let put = c.find(e, *k, OptionKind::Put);
            let mut cells = cells_for(call, *k < c.spot, false);
            let is_atm = atm.is_some_and(|a| (a - k).abs() < 1e-9);
            let kcell = Cell::new(format!("{k:.2}")).color(if is_atm { p.yellow } else { p.text }).bg(if is_atm { p.yellow.opacity(0.15) } else { p.surface });
            cells.push(kcell);
            cells.extend(cells_for(put, *k > c.spot, true));
            let row = table_row(SharedString::from(format!("ch-{i}")), &cols, cells, i % 2 == 1, cx);
            if is_atm {
                row.border_t_1().border_color(p.yellow.opacity(0.6))
            } else {
                row
            }
        });
        v_flex()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(table_header(&cols, cx).w(px(total)))
            .child(div().id("chain-rows").flex_1().min_h_0().overflow_y_scroll().track_scroll(&self.scroll).child(v_flex().w(px(total)).children(rows)))
            .into_any_element()
    }

    fn render_analytics(&self, c: &OptionChain, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let a = self.analytics.clone();
        let e = self.expiry;
        let d0 = c.as_of_date();
        let mut body = panel_body()
            .child(kv("標的 / 現價", format!("{} @ {:.2}", c.underlying, c.spot), cx))
            .child(kv("快照時間", c.as_of.format("%Y-%m-%d %H:%M").to_string(), cx))
            .child(kv("來源", self.label.clone(), cx))
            .child(kv("報價數 / 到期日", format!("{} / {}", c.quotes.len(), c.expiries().len()), cx));
        if let (Some(a), Some(e)) = (a.as_ref(), e) {
            let dte = (e - d0).num_days();
            body = body.child(section_title(format!("到期 {e} ({dte} DTE)"), cx));
            if let Some(m) = a.expected_moves.iter().find(|m| m.expiry == e) {
                body = body
                    .child(kv("ATM 跨式 Straddle", f2(m.straddle), cx))
                    .child(kv_colored("預期波動 Expected Move", format!("±{} (±{:.2})", pct(m.move_pct), m.straddle), p.yellow, cx))
                    .child(kv("預期區間", format!("{:.2} – {:.2}", m.lower, m.upper), cx))
                    .child(kv("ATM IV", opt_pct(m.atm_iv), cx));
            }
            if let Some(f) = a.surface.fit_for(e) {
                body = body.child(kv("擬合 ATM IV / RMSE", format!("{} / {:.2} 點", pct(f.atm_iv()), f.rmse * 100.0), cx));
            }
            if let Some(s) = a.surface.skew_25d(e) {
                body = body.child(kv_colored("25Δ 偏斜 (Put−Call)", format!("{:+.2} 點", s * 100.0), if s > 0.0 { p.red } else { p.green }, cx));
            }
            if let Some(pe) = &self.per_exp {
                body = body
                    .child(kv("P/C 成交量比", opt_f2(pe.pcr.volume_ratio), cx))
                    .child(kv("P/C 未平倉比", opt_f2(pe.pcr.oi_ratio), cx))
                    .child(kv("最大痛點 Max Pain", pe.max_pain.as_ref().map(|m| f2(m.strike)).unwrap_or_else(|| "—".into()), cx))
                    .child(kv_colored("Net GEX ($/1%)", money(pe.gex.total_gex), sign_color(pe.gex.total_gex), cx))
                    .child(kv("Gamma flip", pe.gex.gamma_flip.map(f2).unwrap_or_else(|| "—".into()), cx))
                    .child(kv("微笑點數", pe.smile.len().to_string(), cx));
            }
            body = body.child(section_title("全部到期", cx));
            body = body
                .child(kv("P/C 成交量比 (全)", opt_f2(a.put_call_total.volume_ratio), cx))
                .child(kv("P/C 未平倉比 (全)", opt_f2(a.put_call_total.oi_ratio), cx))
                .child(kv_colored("Net GEX 全部", money(a.gex.total_gex), sign_color(a.gex.total_gex), cx));
        }
        if c.source == "demo" {
            body = body.child(badge("合成示範資料：由內建歷史收盤 + 波動率指數 + 偏斜曲面生成，非真實報價", p.yellow));
        }
        body
    }

    fn term_chart(&self) -> Option<Rc<ChartSpec>> {
        let a = self.analytics.as_ref()?;
        let p = palette();
        let ts: Vec<(f64, f64)> = a.term_structure.iter().map(|(d, v)| (*d as f64, *v)).collect();
        let sk: Vec<(f64, f64)> = a
            .skew_25d
            .iter()
            .map(|(e, s)| (((*e - a.as_of).num_days()) as f64, *s))
            .collect();
        let mut spec = ChartSpec::new()
            .y_fmt(fmt_pct)
            .x_fmt(|v| format!("{v:.0}d"))
            .series(Series::line("ATM IV 期限結構", ts.clone(), p.blue).width(1.6))
            .series(Series::points("", ts, p.blue).no_legend())
            .series(Series::line("25Δ 偏斜", sk, p.orange).width(1.2));
        if let Some(e) = self.expiry {
            spec = spec.vline(RefLine::new((e - a.as_of).num_days() as f64, p.yellow));
        }
        Some(spec.include_zero().into_rc())
    }
}

impl Render for ChainPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx).into_any_element();
        let p = palette();
        let Some(c) = self.chain.clone() else {
            let msg = if self.loading {
                "背景載入中…".to_string()
            } else if let Some(e) = &self.error {
                format!("載入失敗：\n{e}")
            } else {
                "選擇資料來源並按「取得」。此雲端環境無法連線至 CBOE/Yahoo，可用「最新快照」「CSV」或「合成示範資料」離線操作。".into()
            };
            return v_flex()
                .size_full()
                .child(toolbar)
                .child(
                    v_flex()
                        .p_4()
                        .gap_2()
                        .child(div().text_size(px(12.0)).text_color(if self.error.is_some() { p.red } else { p.muted }).font_family(MONO_FONT).children(msg.lines().map(|l| div().child(l.to_string()))))
                        .child(h_flex().child(Button::new("demo2").small().label("載入合成示範資料 (QQQ)").on_click(cx.listener(|this, _, window, cx| this.load_demo(window, cx))))),
                )
                .into_any_element();
        };
        let expiries = self.render_expiries(&c, cx).into_any_element();
        let table = self.render_table(&c, cx);
        let analytics = self.render_analytics(&c, cx);
        let term = self.term_chart();
        let (smile, gex) = self.per_exp.as_ref().map(|p| (p.smile_chart.clone(), p.gex_chart.clone())).unzip();
        v_flex()
            .size_full()
            .child(toolbar)
            .when_some(self.error.clone(), |d, e| d.child(div().px_3().text_size(px(11.0)).text_color(p.red).child(e.lines().next().unwrap_or("").to_string())))
            .child(expiries)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .gap_2()
                    .px_3()
                    .pb_2()
                    .child(panel(format!("{} T 字報價 · 藍底 = 價內 ITM · 黃 = ATM", c.underlying), cx).h_full().flex_none().child(table))
                    .child(
                        div().id("chain-right").flex_1().min_w_0().h_full().overflow_y_scroll().child(
                            v_flex()
                                .gap_2()
                                .child(h_flex().gap_2().items_start().child(panel("分析 Analytics", cx).w(px(300.0)).flex_none().child(analytics)).child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_2()
                                        .child(panel("IV 微笑 Smile (選定到期)", cx).h(px(250.0)).child(div().flex_1().min_h_0().p_1().children(smile.map(xy_chart))))
                                        .child(panel("ATM 期限結構 & 25Δ 偏斜 (x = DTE)", cx).h(px(230.0)).child(div().flex_1().min_h_0().p_1().children(term.map(xy_chart)))),
                                ))
                                .child(panel("Gamma Exposure by Strike (選定到期，綠=正 GEX)", cx).h(px(240.0)).child(div().flex_1().min_h_0().p_1().children(gex.map(xy_chart)))),
                        ),
                    ),
            )
            .into_any_element()
    }
}
