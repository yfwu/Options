//! 函數工作室 Payoff Studio: write a payoff function f(S) in the payoff language, compile
//! it to cash + stock + calls + puts (static replication), price it on a synthetic smile
//! or the loaded option chain and see target vs replicated payoff, today's value, the
//! Q / P distributions and the per-leg decomposition ("function = sum of primitives").

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDate;
use gpui::{div, prelude::*, px, ClipboardItem, Context, Entity, Hsla, SharedString, Subscription, Window};
use gpui_component::{
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectState},
    slider::{Slider, SliderEvent, SliderState},
    v_flex, ActiveTheme, IndexPath, Sizable,
};
use optlab_core::payoff::{compile_book, date_view, examples, DateView, parse, price, value_curve, CompileOptions, Instrument, LegStyle, Market, PayoffStats, PricedPayoff, Replication, SyntheticMarket};
use optlab_core::strategy::{Leg, Strategy};
use optlab_core::{OptionChain, OptionKind};
use optlab_data::{bundled, presets};

use crate::data_bridge;
use crate::state::{AppEvent, AppState, StatusLevel};
use crate::theme::{palette, MONO_FONT};
use crate::widgets::charts::{fmt_auto, xy_chart, ChartSpec, RefLine, Series, SeriesKind};
use crate::widgets::ui::*;

const DEFAULT_SRC: &str = "2*bull(100%, 115%)";
const DEBOUNCE_MS: u64 = 250;
const CHART_LEFT: f32 = 62.0;

const STYLES: [(LegStyle, &str); 4] = [
    (LegStyle::Minimal, "最少部位"),
    (LegStyle::OutOfTheMoney, "價外優先"),
    (LegStyle::AllCalls, "全買權"),
    (LegStyle::AllPuts, "全賣權"),
];

const REFERENCE: &[(&str, &str)] = &[
    ("S · spot · 110%", "到期價格 · 現價 · 1.10×現價"),
    ("+ − * / ^ ( )", "四則、次方；2call(K) = 2*call(K)"),
    ("call(K) put(K)", "(S−K)⁺ · (K−S)⁺"),
    ("digital(K) digitalput(K)", "S>K 得 1 · S<K 得 1（以窄價差近似）"),
    ("pwl(x1,y1, x2,y2, …)", "用點畫折線（兩端水平延伸）"),
    ("straddle(K) strangle(Kp,Kc)", "跨式 · 勒式"),
    ("bull(K1,K2) bear(K1,K2)", "多頭 / 空頭價差（寬度 K2−K1）"),
    ("butterfly(K1,K2,K3) condor(K1..K4)", "蝶式 · 兀鷹"),
    ("collar(Kp,Kc) protect(K)", "股票+領口 · 股票+保護賣權"),
    ("max min abs pos neg", "max/min 可多參數；pos(x)=x⁺"),
    ("clamp(x,lo,hi) sqrt ln exp", "非線性函數 → 以履約價網格近似"),
    ("a = 600; call(a)", "變數；以 ; 或換行分隔，# 註解"),
    ("expr @ 45 · @ 6m · @ 1y", "跨到期日：替子函數指定到期（天/週 w/月 m/年 y）；未標記者用 DTE 欄位"),
    ("call(80%) @ 540 − call(105%) @ 45", "不同到期日只能線性組合（+ − ×常數），不可放進 max/min 等非線性函數"),
];

/// Everything the background job needs (Send).
struct Job {
    src: String,
    sym: String,
    spot: Option<f64>,
    dte: i64,
    r: f64,
    q: f64,
    skew: f64,
    iv: Option<f64>,
    long_iv: Option<f64>,
    units: f64,
    mu: f64,
    style: LegStyle,
    chain: Option<Arc<OptionChain>>,
}

/// Result of one compile + price (Send).
struct Out {
    src: String,
    sym: String,
    rep: Replication,
    priced: PricedPayoff,
    market_label: String,
    from_chain: bool,
    created: NaiveDate,
    lo: f64,
    hi: f64,
    target: Vec<(f64, f64)>,
    repl: Vec<(f64, f64)>,
    now: Vec<(f64, f64)>,
    mid: Vec<(f64, f64)>,
    legs: Vec<(String, Vec<(f64, f64)>)>,
    /// Per priced leg: expiry label and date.
    leg_exp: Vec<(String, NaiveDate)>,
    /// Market used for pricing (needed by the time slider).
    mkt: Market,
    /// Plotting grid for the date view.
    xs: Vec<f64>,
}

struct Charts {
    main: Rc<ChartSpec>,
    density: Rc<ChartSpec>,
    decomp: Rc<ChartSpec>,
}

struct Inputs {
    spot: Entity<InputState>,
    dte: Entity<InputState>,
    iv: Entity<InputState>,
    long_iv: Entity<InputState>,
    r: Entity<InputState>,
    q: Entity<InputState>,
    skew: Entity<InputState>,
    units: Entity<InputState>,
    mu: Entity<InputState>,
}

pub struct StudioPage {
    state: Entity<AppState>,
    editor: Entity<InputState>,
    symbol: Entity<SelectState<Vec<SharedString>>>,
    inp: Inputs,
    style: LegStyle,
    use_chain: bool,
    errors: FormErrors,
    error: Option<String>,
    out: Option<Arc<Out>>,
    charts: Option<Charts>,
    // --- time slider
    day_slider: Entity<SliderState>,
    iv_slider: Entity<SliderState>,
    day: f64,
    dvol: f64,
    pnl_mode: bool,
    view: Option<DateView>,
    view_gen: u64,
    playing: u64,
    play_seq: u64,
    _slider_subs: Vec<Subscription>,
    computing: bool,
    edit_gen: u64,
    comp_gen: u64,
    _subs: Vec<Subscription>,
}

fn latest_tbill() -> f64 {
    match presets::rate_source() {
        optlab_core::backtest::RateSource::Series(s) => s.last().map_or(0.04, |x| x.1 / 100.0),
        optlab_core::backtest::RateSource::Constant(c) => c,
    }
}

fn instrument_name(i: Instrument) -> String {
    match i {
        Instrument::Cash => "零息債/現金".into(),
        Instrument::Stock => "股票".into(),
        Instrument::Call(k) => format!("Call {k:.2}"),
        Instrument::Put(k) => format!("Put {k:.2}"),
    }
}

/// The heavy part, run on the background executor.
fn run_job(job: Job) -> Result<Out, String> {
    let prog = parse(&job.src).map_err(|e| format!("{e:#}"))?;
    let today = chrono::Local::now().date_naive();
    let (mkt, spot, label, from_chain, created, expiry) = match &job.chain {
        Some(ch) => {
            let exp = ch.expiry_near_dte(job.dte).ok_or("期權鏈沒有可用的到期日")?;
            let d0 = ch.as_of_date();
            let label = format!(
                "期權鏈：{} 現價 {:.2} · 到期 {exp} ({} 天) · r {:.2}% · q {:.2}% · 來源 {}",
                ch.underlying,
                ch.spot,
                (exp - d0).num_days(),
                job.r * 100.0,
                job.q * 100.0,
                ch.source
            );
            let s = ch.spot;
            (Market::chain((**ch).clone(), exp, job.r, job.q), s, label, true, d0, exp)
        }
        None => {
            let s = match job.spot {
                Some(s) => s,
                None => bundled::prices(&job.sym).and_then(|p| p.bars.last().map(|b| b.close)).ok_or_else(|| format!("沒有 {} 的內建價格：請輸入現價", job.sym))?,
            };
            let t = job.dte.max(1) as f64 / 365.0;
            let vm = presets::vol_model_for(&job.sym);
            let vix_now = bundled::default_vol_index_for(&job.sym).and_then(|v| v.last()).map(|(_, v)| v);
            let (iv, iv_src) = match (job.iv, vix_now) {
                (Some(v), _) => (v, "手動".to_string()),
                (None, Some(v)) => (vm.atm_vol(t, v), bundled::default_vol_index_name(&job.sym).to_string()),
                (None, None) => (0.2, "預設".to_string()),
            };
            // ATM term structure: 2-year IV from the same vol model (keeps the ratio when IV is overridden)
            let long_iv = job.long_iv.unwrap_or_else(|| vix_now.map(|v| vm.atm_vol(2.0, v) * iv / vm.atm_vol(t, v).max(1e-6)).unwrap_or(iv));
            let m = SyntheticMarket { skew: job.skew, long_iv, long_t: 2.0, ..SyntheticMarket::new(s, t, job.r, job.q, iv) };
            let exp = today + chrono::Duration::days(job.dte.max(1));
            let label = format!(
                "合成市場：{} 現價 {s:.2} · ATM IV {:.1}% ({iv_src}, {} 天) → {:.1}% (2 年) · 偏斜 {:.2} · r {:.2}% · q {:.2}% · 預設到期 {} 天 ({exp})",
                job.sym,
                iv * 100.0,
                job.dte.max(1),
                long_iv * 100.0,
                job.skew,
                job.r * 100.0,
                job.q * 100.0,
                job.dte.max(1)
            );
            (Market::Synthetic(m), s, label, false, today, exp)
        }
    };
    let style = job.style;
    let opts_for = |days: f64| {
        let mut opt = match mkt.listed_strikes_for(days) {
            Some(ks) if !ks.is_empty() => CompileOptions::listed(spot, ks),
            _ => CompileOptions::free(spot),
        };
        opt.style = style;
        opt
    };
    let rep = compile_book(&prog, mkt.default_days(), &opts_for).map_err(|e| format!("{e:#}"))?;
    let priced = price(&prog, &rep, &mkt, job.units, job.mu);
    let (lo, hi) = (spot * 0.4, spot * 1.8);
    let n = 361;
    let xs: Vec<f64> = (0..n).map(|i| lo + (hi - lo) * i as f64 / (n - 1) as f64).collect();
    let mut grid = xs.clone();
    grid.extend(rep.kinks.iter().copied().filter(|k| *k > lo && *k < hi));
    grid.sort_by(f64::total_cmp);
    grid.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    // values at the horizon (= the expiry for single-expiry books)
    let target = grid.iter().map(|&x| (x, priced.horizon_target(&prog, &rep, x))).collect();
    let repl = grid.iter().map(|&x| (x, priced.horizon_value(x))).collect();
    let t = priced.t;
    let now = value_curve(&priced, &xs, t);
    let mid = value_curve(&priced, &xs, t / 2.0);
    let default_days = mkt.default_days();
    let leg_exp: Vec<(String, NaiveDate)> = priced
        .legs
        .iter()
        .map(|l| {
            let days = l.leg.days.unwrap_or(default_days);
            let date = mkt.expiry_for_days(days).unwrap_or_else(|| created + chrono::Duration::days(days.round() as i64));
            let label = if from_chain { date.format("%y-%m-%d").to_string() } else { format!("{:.0}d", (l.t * 365.0).round()) };
            (label, date)
        })
        .collect();
    let legs = priced
        .legs
        .iter()
        .zip(&leg_exp)
        .map(|(l, (el, _))| {
            let name = format!("{:+.2}×{} @{el}", l.leg.qty, instrument_name(l.leg.instrument));
            (name, grid.iter().map(|&x| (x, l.value_at(x, t, priced.r, priced.q))).collect())
        })
        .collect();
    let _ = expiry;
    Ok(Out { src: job.src, sym: job.sym, rep, priced, market_label: label, from_chain, created, lo, hi, target, repl, now, mid, legs, leg_exp, mkt, xs })
}

impl StudioPage {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| InputState::new(window, cx).code_editor("text").line_number(true).default_value(DEFAULT_SRC).placeholder("輸入 payoff 函數 f(S)，例如 2*bull(100%, 115%)"));
        let syms: Vec<SharedString> = data_bridge::bundled_symbols().into_iter().map(SharedString::from).collect();
        let default = state.read(cx).settings.default_symbol.to_ascii_uppercase();
        let idx = syms.iter().position(|s| s.as_ref() == default).unwrap_or(0);
        let sym = syms.get(idx).map(|s| s.to_string()).unwrap_or_else(|| "QQQ".into());
        let symbol = cx.new(|cx| SelectState::new(syms, Some(IndexPath::default().row(idx)), window, cx));
        let inp = Inputs {
            spot: new_input_ph("", "最新收盤", window, cx),
            dte: new_input("120", window, cx),
            iv: new_input_ph("", "自動", window, cx),
            long_iv: new_input_ph("", "自動", window, cx),
            r: new_input(format!("{:.2}", latest_tbill() * 100.0), window, cx),
            q: new_input(format!("{:.2}", presets::default_dividend_yield(&sym) * 100.0), window, cx),
            skew: new_input("0.30", window, cx),
            units: new_input("100", window, cx),
            mu: new_input("8", window, cx),
        };
        let mut subs = vec![cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::Change = ev {
                this.schedule(cx);
            }
        })];
        for e in [&inp.spot, &inp.dte, &inp.iv, &inp.long_iv, &inp.r, &inp.q, &inp.skew, &inp.units, &inp.mu] {
            subs.push(cx.subscribe_in(e, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.schedule(cx);
                }
            }));
        }
        subs.push(cx.subscribe_in(&symbol, window, |this, _, ev: &SelectEvent<Vec<SharedString>>, window, cx| {
            let SelectEvent::Confirm(Some(v)) = ev else { return };
            let q = presets::default_dividend_yield(v);
            this.inp.q.update(cx, |s, cx| s.set_value(format!("{:.2}", q * 100.0), window, cx));
            this.schedule(cx);
        }));
        let mut this = Self {
            state,
            editor,
            symbol,
            inp,
            style: LegStyle::Minimal,
            use_chain: false,
            errors: FormErrors::default(),
            error: None,
            out: None,
            charts: None,
            computing: false,
            edit_gen: 0,
            comp_gen: 0,
            _subs: subs,
            day_slider: cx.new(|_| SliderState::new().min(0.0).max(1.0).step(1.0).default_value(0.0)),
            iv_slider: cx.new(|_| SliderState::new().min(-20.0).max(20.0).step(0.5).default_value(0.0)),
            day: 0.0,
            dvol: 0.0,
            pnl_mode: false,
            view: None,
            view_gen: 0,
            playing: 0,
            play_seq: 0,
            _slider_subs: vec![],
        };
        this.reset_sliders(1.0, cx);
        this.recompile(cx);
        this
    }

    fn horizon_days(&self) -> f64 {
        self.out.as_ref().map(|o| (o.priced.t * 365.0).round().max(1.0)).unwrap_or(1.0)
    }

    /// (Re)create the date slider for a new horizon (day back to 0; the IV shift is kept).
    fn reset_sliders(&mut self, horizon: f64, cx: &mut Context<Self>) {
        let h = horizon.max(1.0) as f32;
        self.day_slider = cx.new(|_| SliderState::new().min(0.0).max(h).step(1.0).default_value(0.0));
        self.day = 0.0;
        self.playing = 0;
        let mut subs = vec![cx.subscribe(&self.day_slider, |this, _, ev: &SliderEvent, cx| {
            let SliderEvent::Change(v) = ev;
            this.playing = 0;
            this.day = v.start() as f64;
            this.schedule_view(cx);
        })];
        if self._slider_subs.is_empty() || self._slider_subs.len() < 2 {
            subs.push(cx.subscribe(&self.iv_slider, |this, _, ev: &SliderEvent, cx| {
                let SliderEvent::Change(v) = ev;
                this.playing = 0;
                this.dvol = v.start() as f64;
                this.schedule_view(cx);
            }));
        } else {
            subs.push(self._slider_subs.remove(1));
        }
        self._slider_subs = subs;
    }

    fn set_day(&mut self, d: f64, window: &mut Window, cx: &mut Context<Self>) {
        let d = d.clamp(0.0, self.horizon_days()).round();
        self.day = d;
        self.day_slider.update(cx, |s, cx| s.set_value(d as f32, window, cx));
        self.schedule_view(cx);
    }

    fn set_dvol(&mut self, v: f64, window: &mut Window, cx: &mut Context<Self>) {
        self.dvol = v;
        self.iv_slider.update(cx, |s, cx| s.set_value(v as f32, window, cx));
        self.schedule_view(cx);
    }

    /// Animate the date slider from the current day to the horizon (~3 s, 30 frames).
    fn toggle_play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playing != 0 {
            self.playing = 0;
            cx.notify();
            return;
        }
        self.play_seq += 1;
        let token = self.play_seq;
        self.playing = token;
        let h = self.horizon_days();
        let start = if self.day >= h { 0.0 } else { self.day };
        cx.spawn_in(window, async move |this, cx| {
            for i in 1..=30 {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let go = this
                    .update_in(cx, |this, window, cx| {
                        if this.playing != token {
                            return false;
                        }
                        this.set_day(start + (h - start) * i as f64 / 30.0, window, cx);
                        true
                    })
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if this.playing == token {
                    this.playing = 0;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    /// Recompute the date view (background, stale results dropped).
    fn schedule_view(&mut self, cx: &mut Context<Self>) {
        let Some(out) = self.out.clone() else { return };
        self.view_gen += 1;
        let g = self.view_gen;
        let (day, dv) = (self.day, self.dvol / 100.0);
        let task = cx.background_executor().spawn(async move {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| date_view(&out.priced, &out.mkt, &out.xs, day, dv))).ok()
        });
        cx.spawn(async move |this, cx| {
            let v = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.view_gen != g {
                    return;
                }
                this.view = v;
                this.rebuild_main();
                cx.notify();
            });
        })
        .detach();
    }

    fn rebuild_main(&mut self) {
        if let (Some(o), Some(ch)) = (self.out.as_ref(), self.charts.as_mut()) {
            ch.main = Self::main_chart(o, self.view.as_ref(), self.pnl_mode, self.day, self.dvol);
        }
    }

    /// Debounced recompile.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        self.playing = 0;
        self.edit_gen += 1;
        let g = self.edit_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(DEBOUNCE_MS)).await;
            let _ = this.update(cx, |this, cx| {
                if this.edit_gen == g {
                    this.recompile(cx);
                }
            });
        })
        .detach();
    }

    pub fn recompile(&mut self, cx: &mut Context<Self>) {
        let mut e = FormErrors::default();
        let i = &self.inp;
        let opt_num = |e: &mut FormErrors, key: &'static str, inp: &Entity<InputState>, lo: f64, hi: f64| -> Option<f64> {
            if input_text(inp, cx).is_empty() {
                None
            } else {
                Some(e.num_range(key, inp, lo, hi, cx))
            }
        };
        let spot = opt_num(&mut e, "spot", &i.spot, 0.0001, 1e9);
        let iv = opt_num(&mut e, "iv", &i.iv, 1.0, 300.0).map(|v| v / 100.0);
        let long_iv = opt_num(&mut e, "long_iv", &i.long_iv, 1.0, 300.0).map(|v| v / 100.0);
        let job = Job {
            src: self.editor.read(cx).value().to_string(),
            sym: self.symbol.read(cx).selected_value().map(|s| s.to_string()).unwrap_or_else(|| "QQQ".into()),
            spot,
            dte: e.int("dte", &i.dte, 1, 3650, cx),
            r: e.num_range("r", &i.r, -10.0, 50.0, cx) / 100.0,
            q: e.num_range("q", &i.q, -10.0, 50.0, cx) / 100.0,
            skew: e.num_range("skew", &i.skew, -3.0, 3.0, cx),
            iv,
            long_iv,
            units: e.num_range("units", &i.units, 0.0001, 1e9, cx),
            mu: e.num_range("mu", &i.mu, -100.0, 300.0, cx) / 100.0,
            style: self.style,
            chain: if self.use_chain { self.state.read(cx).chain.clone() } else { None },
        };
        let ok = e.is_empty();
        self.errors = e;
        if !ok {
            cx.notify();
            return;
        }
        if job.src.trim().is_empty() {
            self.error = Some("請輸入 payoff 函數，例如 call(100%) 或 2*bull(100%, 115%)".into());
            cx.notify();
            return;
        }
        self.comp_gen += 1;
        let g = self.comp_gen;
        self.computing = true;
        let task = cx.background_executor().spawn(async move {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(job))).unwrap_or_else(|_| Err("內部錯誤：計算時發生 panic（請回報此運算式）".into()))
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.comp_gen != g {
                    return; // stale
                }
                this.computing = false;
                match res {
                    Ok(out) => {
                        let out = Arc::new(out);
                        this.charts = Some(Self::make_charts(&out, this.pnl_mode, this.dvol));
                        this.out = Some(out);
                        this.error = None;
                        this.view = None;
                        let h = this.horizon_days();
                        this.reset_sliders(h, cx);
                        this.schedule_view(cx);
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn main_chart(o: &Out, view: Option<&DateView>, pnl: bool, day: f64, dvol: f64) -> Rc<ChartSpec> {
        let p = palette();
        let pr = &o.priced;
        let fin = |v: &[(f64, f64)], shift: f64| -> Vec<(f64, f64)> { v.iter().copied().filter(|(_, y)| y.is_finite()).map(|(x, y)| (x, y - shift)).collect() };
        let grow = (pr.r * pr.t).exp();
        let cost_ref = pr.price_mid * grow;
        // in P&L mode every curve is shown net of the cost grown to its own date
        let (s_h, s_now, s_mid) = if pnl { (cost_ref, pr.price_mid, pr.price_mid * (pr.r * pr.t / 2.0).exp()) } else { (0.0, 0.0, 0.0) };
        let moved = day > 0.0 || dvol.abs() > 1e-9;
        let dim = |c: Hsla| if moved { c.opacity(0.4) } else { c };
        let mut main = ChartSpec::new()
            .x_fmt(fmt_auto)
            .x_range(o.lo, o.hi)
            .min_left(CHART_LEFT)
            .series(Series::line(if pr.multi_expiry { format!("目標 @ {:.0} 天", pr.horizon_days) } else { "目標 f(S)".to_string() }, fin(&o.target, s_h), dim(p.blue)).width(2.4))
            .series(Series::line(if pr.multi_expiry { format!("複製部位 @ {:.0} 天", pr.horizon_days) } else { "複製 g(S) 到期".to_string() }, fin(&o.repl, s_h), dim(p.orange)).width(1.3).dashed())
            .series(Series::line("今日價值", fin(&o.now, s_now), dim(p.yellow)).width(1.2))
            .series(Series::line("半途價值 (t/2)", fin(&o.mid, s_mid), dim(p.purple)).width(1.0))
            .vline(RefLine::new(pr.spot, p.green).label(format!("現價 {:.2}", pr.spot)));
        if pnl {
            main = main.hline(RefLine::new(0.0, p.red).solid().label("損益 0"));
        } else {
            main = main.hline(RefLine::new(cost_ref, p.red).label(format!("損益平衡線 成本×e^rT = {cost_ref:.2}")));
        }
        if let Some(v) = view {
            let pts: Vec<(f64, f64)> = v.curve.iter().filter(|c| c.1.is_finite()).map(|c| (c.0, if pnl { c.2 } else { c.1 })).collect();
            let label = if v.elapsed_days <= 0.0 { "選定日期：今天".to_string() } else { format!("選定日期：第 {:.0} 天", v.elapsed_days) };
            let label = if v.vol_shift.abs() > 1e-9 { format!("{label} · IV {:+.1} 點", v.vol_shift * 100.0) } else { label };
            main = main.series(Series::line(label, pts, p.cyan).width(3.0));
            let y = if pnl { v.pnl_at_spot } else { v.value_at_spot };
            if y.is_finite() {
                main = main.series(Series { kind: SeriesKind::Points { radius: 5.0 }, ..Series::points(format!("現價處 {y:.2}"), vec![(pr.spot, y)], p.cyan) });
            }
            for b in &v.breakevens {
                if moved {
                    main = main.vline(RefLine::new(*b, p.cyan.opacity(0.7)).label(format!("BE {b:.2}")));
                }
            }
        }
        if o.rep.kinks.len() <= 25 {
            for k in &o.rep.kinks {
                if *k > o.lo && *k < o.hi {
                    main = main.vline(RefLine::new(*k, p.border));
                }
            }
        }
        if !moved {
            for b in &pr.breakevens {
                main = main.vline(RefLine::new(*b, p.red).label(format!("BE {b:.2}")));
            }
        }
        main.into_rc()
    }

    fn make_charts(o: &Out, pnl: bool, dvol: f64) -> Charts {
        let p = palette();
        let pr = &o.priced;
        let fin = |v: &[(f64, f64)]| -> Vec<(f64, f64)> { v.iter().copied().filter(|(_, y)| y.is_finite()).collect() };
        let main = Self::main_chart(o, None, pnl, 0.0, dvol);
        // densities as probability per 1% of spot
        let scale = pr.spot * 0.01;
        let dq: Vec<(f64, f64)> = pr.density.iter().filter(|d| d.0 >= o.lo && d.0 <= o.hi).map(|d| (d.0, d.1 * scale)).collect();
        let dp: Vec<(f64, f64)> = pr.density.iter().filter(|d| d.0 >= o.lo && d.0 <= o.hi).map(|d| (d.0, d.2 * scale)).collect();
        let mut density = ChartSpec::new()
            .x_fmt(fmt_auto)
            .y_fmt(|v| format!("{:.1}%", v * 100.0))
            .x_range(o.lo, o.hi)
            .min_left(CHART_LEFT)
            .series(Series::area("風險中性 q_Q", dq, p.blue, 0.0).width(1.3))
            .series(Series::area(format!("真實世界 q_P (μ {:.0}%)", pr.mu * 100.0), dp, p.green, 0.0).width(1.3))
            .vline(RefLine::new(pr.spot, p.green));
        for b in &pr.breakevens {
            density = density.vline(RefLine::new(*b, p.red));
        }
        let cyc = [p.blue, p.green, p.orange, p.purple, p.cyan, p.red, p.yellow, p.muted];
        let many = o.legs.len() > 10;
        let mut decomp = ChartSpec::new().x_fmt(fmt_auto).x_range(o.lo, o.hi).min_left(CHART_LEFT);
        for (i, (name, pts)) in o.legs.iter().enumerate() {
            let mut sr = Series::line(name.clone(), pts.clone(), cyc[i % cyc.len()].opacity(0.85)).width(1.0);
            if many {
                sr = sr.no_legend();
            }
            decomp = decomp.series(sr);
        }
        decomp = decomp.series(Series::line("合計 g(S)", fin(&o.repl), p.text).width(2.2)).hline(RefLine::new(0.0, p.dim).solid());
        Charts { main, density: density.into_rc(), decomp: decomp.into_rc() }
    }

    fn load_example(&mut self, src: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |s, cx| s.set_value(src, window, cx));
        self.schedule(cx);
    }

    fn copy_formula(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &self.out else { return };
        let f = format!("f(S) = {}\n≈ {}", o.src.replace('\n', "; "), o.rep.formula());
        cx.write_to_clipboard(ClipboardItem::new_string(f));
        self.state.update(cx, |st, cx| st.set_status(StatusLevel::Ok, "已複製公式到剪貼簿", cx));
    }

    fn export_csv(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &self.out else { return };
        let svc = self.state.read(cx).svc.clone();
        let dir = svc.store().root().join("exports");
        let path = dir.join(format!("payoff_{}_{}.csv", o.sym, chrono::Local::now().format("%Y%m%d_%H%M%S")));
        let mut s = format!("# f(S) = {}\n# g(S) = {}\nS,target,replicated,value_now,value_mid\n", o.src.replace('\n', "; "), o.rep.formula());
        let lookup = |v: &[(f64, f64)], x: f64| v.iter().find(|p| (p.0 - x).abs() < 1e-9).map(|p| p.1).unwrap_or(f64::NAN);
        for (x, now) in &o.now {
            s.push_str(&format!("{x:.4},{:.6},{:.6},{now:.6},{:.6}\n", lookup(&o.target, *x), lookup(&o.repl, *x), lookup(&o.mid, *x)));
        }
        let res = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, s));
        self.state.update(cx, |st, cx| match res {
            Ok(()) => st.set_status(StatusLevel::Ok, format!("已匯出 CSV：{}", path.display()), cx),
            Err(e) => st.set_status(StatusLevel::Error, format!("匯出失敗：{e}"), cx),
        });
    }

    fn send_to_strategy_lab(&mut self, cx: &mut Context<Self>) {
        let Some(o) = &self.out else { return };
        let pr = &o.priced;
        let mut strat = Strategy::new(format!("函數工作室：{}", o.src.replace('\n', "; ")), o.sym.clone(), o.created);
        let mut skipped_cash = false;
        for (l, (_, exp)) in pr.legs.iter().zip(&o.leg_exp) {
            let exp = *exp;
            let n = l.leg.qty * pr.units;
            match l.leg.instrument {
                Instrument::Call(k) => strat.add_leg(Leg::option(OptionKind::Call, k, exp, n / 100.0, l.mid, l.iv)),
                Instrument::Put(k) => strat.add_leg(Leg::option(OptionKind::Put, k, exp, n / 100.0, l.mid, l.iv)),
                Instrument::Stock => strat.add_leg(Leg::stock(n, pr.spot)),
                Instrument::Cash => skipped_cash = true,
            }
        }
        if strat.legs.is_empty() {
            self.state.update(cx, |st, cx| st.set_status(StatusLevel::Warn, "此函數只有現金部位，沒有可送到策略實驗室的選擇權/股票", cx));
            return;
        }
        let label = o.src.replace('\n', "; ");
        let (spot, r, q) = (pr.spot, pr.r, pr.q);
        self.state.update(cx, |st, cx| {
            if skipped_cash {
                st.set_status(StatusLevel::Info, "已送到策略實驗室（零息債/現金部位省略，只影響利息）", cx);
            }
            cx.emit(AppEvent::OpenInStrategyLab(Box::new((strat, spot, r, q, label))));
        });
    }

    fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let muted = cx.theme().muted_foreground;
        let current = self.editor.read(cx).value().to_string();
        v_flex()
            .gap_2()
            .p_2()
            .child(
                panel_with(
                    "f(S) =  程式碼",
                    div().text_size(px(10.5)).text_color(p.dim).child(if self.computing { "編譯中…" } else { "自動編譯 (250ms)" }),
                    cx,
                )
                .child(div().h(px(190.0)).font_family(MONO_FONT).child(Input::new(&self.editor).h_full().bordered(false))),
            )
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .bg(p.red.opacity(0.12))
                        .border_1()
                        .border_color(p.red.opacity(0.5))
                        .text_size(px(11.5))
                        .font_family(MONO_FONT)
                        .text_color(p.red)
                        .children(e.lines().map(|l| div().child(l.to_string()))),
                )
            })
            .child(
                panel("範例 Examples（點選載入）", cx).child(v_flex().p_1().gap_0p5().children(examples().into_iter().enumerate().map(|(i, (name, src, note))| {
                    let active = current.trim() == src.trim();
                    v_flex()
                        .id(SharedString::from(format!("ex-{i}")))
                        .px_2()
                        .py_1()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .when(active, |d| d.bg(p.blue.opacity(0.16)))
                        .when(!active, |d| d.hover(|s| s.bg(p.elevated)))
                        .on_click(cx.listener(move |this, _, window, cx| this.load_example(src, window, cx)))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_size(px(12.0)).text_color(if active { p.text } else { p.muted }).child(name))
                                .child(div().flex_1().min_w_0().truncate().text_size(px(11.0)).font_family(MONO_FONT).text_color(p.blue).child(src.replace('\n', "; "))),
                        )
                        .child(div().text_size(px(10.5)).text_color(p.dim).child(note))
                }))),
            )
            .child(
                panel("語言參考 Reference", cx).child(v_flex().p_2().gap_1().children(REFERENCE.iter().map(|(a, b)| {
                    v_flex().child(div().text_size(px(11.0)).font_family(MONO_FONT).text_color(p.cyan).child(*a)).child(div().text_size(px(10.5)).text_color(muted).child(*b))
                }))),
            )
    }

    fn render_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let e = &self.errors;
        let i = &self.inp;
        let p = palette();
        let chain = self.state.read(cx).chain.clone();
        let f = |label: &str, key: &str, inp: &Entity<InputState>, w: f32| field_err(label.to_string(), inp, e.get(key), cx).w(px(w));
        v_flex()
            .gap_1p5()
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .flex_wrap()
                    .child(labeled("標的", Select::new(&self.symbol).small(), cx).w(px(104.0)))
                    .child(f("現價", "spot", &i.spot, 84.0))
                    .child(f("DTE 天", "dte", &i.dte, 62.0))
                    .child(f("ATM IV %", "iv", &i.iv, 70.0))
                    .child(f("2 年 ATM IV %", "long_iv", &i.long_iv, 84.0))
                    .child(f("r %", "r", &i.r, 58.0))
                    .child(f("q %", "q", &i.q, 58.0))
                    .child(f("偏斜", "skew", &i.skew, 58.0))
                    .child(f("單位", "units", &i.units, 62.0))
                    .child(f("μ 漂移 %", "mu", &i.mu, 64.0)),
            )
            .child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .child(h_flex().gap_1().child(muted_text("部位風格", cx)).children(STYLES.iter().map(|(s, l)| {
                        let s = *s;
                        chip(SharedString::from(format!("sty-{l}")), *l, self.style == s, cx).on_click(cx.listener(move |this, _, _, cx| {
                            this.style = s;
                            this.recompile(cx);
                        }))
                    })))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(muted_text("市場", cx))
                            .child(chip("src-syn", "合成市場", !self.use_chain, cx).on_click(cx.listener(|this, _, _, cx| {
                                this.use_chain = false;
                                this.recompile(cx);
                            })))
                            .child(
                                chip("src-chain", chain.as_ref().map(|c| format!("期權鏈 {} ({})", c.underlying, c.source)).unwrap_or_else(|| "期權鏈 (未載入)".into()), self.use_chain, cx).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        if this.state.read(cx).chain.is_some() {
                                            this.use_chain = true;
                                            this.recompile(cx);
                                        } else {
                                            this.state.update(cx, |st, cx| st.set_status(StatusLevel::Warn, "尚未載入期權鏈：請先到「期權鏈」頁取得或載入示範資料", cx));
                                        }
                                    }),
                                ),
                            ),
                    ),
            )
            .when(!e.is_empty(), |d| d.child(div().text_size(px(11.0)).text_color(p.red).child(e.summary())))
            .when_some(self.out.as_ref().map(|o| o.market_label.clone()), |d, l| d.child(muted_text(l, cx)))
    }

    fn render_time(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let h = self.horizon_days();
        let Some(o) = self.out.as_ref() else { return div().into_any_element() };
        let date = o.created + chrono::Duration::days(self.day.round() as i64);
        let day_label = format!("第 {:.0} 天 · {date}（距最早到期 {:.0} 天）", self.day, (h - self.day).max(0.0));
        let quick = [("今天", 0.0), ("1/4", 0.25), ("1/2", 0.5), ("3/4", 0.75), ("到期", 1.0)];
        let mut kpis = h_flex().gap_1p5().flex_wrap();
        if let Some(v) = &self.view {
            let u = o.priced.units;
            let g = v.greeks;
            let day0 = v.elapsed_days <= 0.0;
            // on day 0 without an IV shift the P&L is identically ~0 at spot: crossings are noise
            let be = if v.breakevens.is_empty() || (day0 && v.vol_shift == 0.0) {
                "—".to_string()
            } else {
                v.breakevens.iter().map(|b| format!("{b:.1}")).collect::<Vec<_>>().join(" / ")
            };
            kpis = kpis
                .child(kpi_sm("價值@現價 ×單位", money(v.value_at_spot * u), None, cx).min_w(px(104.0)))
                .child(kpi_sm("損益@現價 ×單位", money(v.pnl_at_spot * u), Some(sign_color(v.pnl_at_spot)), cx).min_w(px(104.0)))
                .child(kpi_sm("Δ 股", f1(g.delta * u), None, cx).min_w(px(104.0)))
                .child(kpi_sm("Γ", f3(g.gamma * u), None, cx).min_w(px(104.0)))
                .child(kpi_sm("Θ $/天", f2(g.theta * u), Some(sign_color(g.theta)), cx).min_w(px(104.0)))
                .child(kpi_sm("Vega $/點", f2(g.vega * u), None, cx).min_w(px(104.0)))
                .child(kpi_sm("獲利機率 P / Q", if day0 { "—".to_string() } else { format!("{} / {}", pct(v.prob_profit_p), pct(v.prob_profit_q)) }, None, cx).min_w(px(104.0)))
                .child(kpi_sm("期望損益 P", if day0 { "—".to_string() } else { money(v.expected_pnl_p * u) }, if day0 { None } else { Some(sign_color(v.expected_pnl_p)) }, cx).min_w(px(104.0)))
                .child(kpi_sm("損益兩平", be, Some(p.orange), cx));
        }
        panel_with(
            "時間 & 波動率情境 · 任一日期的組合價值",
            div().text_size(px(10.5)).text_color(p.dim).child("範圍：今天 → 最早到期日（之後的價值取決於路徑）"),
            cx,
        )
        .child(
            v_flex()
                .p_2()
                .gap_1p5()
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .child(div().flex_1().min_w(px(240.0)).text_size(px(12.0)).font_family(MONO_FONT).text_color(p.cyan).child(day_label))
                        .child(h_flex().gap_1().children(quick.iter().map(|(l, f)| {
                            let f = *f;
                            chip(SharedString::from(format!("q-{l}")), *l, (self.day - (h * f).round()).abs() < 0.5, cx).on_click(cx.listener(move |this, _, window, cx| {
                                this.playing = 0;
                                let h = this.horizon_days();
                                this.set_day(h * f, window, cx);
                            }))
                        })))
                        .child(chip("play", if self.playing != 0 { "■ 停止" } else { "▶ 播放" }, self.playing != 0, cx).on_click(cx.listener(|this, _, window, cx| this.toggle_play(window, cx)))),
                )
                .child(div().px_1().child(Slider::new(&self.day_slider)))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(170.0)).flex_none().text_size(px(12.0)).font_family(MONO_FONT).text_color(if self.dvol.abs() > 1e-9 { p.yellow } else { p.muted }).child(format!("IV 變動 {:+.1} 點", self.dvol)))
                        .child(div().flex_1().min_w(px(100.0)).child(Slider::new(&self.iv_slider)))
                        .child(chip("iv-reset", "IV 歸零", false, cx).on_click(cx.listener(|this, _, window, cx| {
                            this.playing = 0;
                            this.set_dvol(0.0, window, cx);
                        }))),
                )
                .child(kpis),
        )
        .into_any_element()
    }

    fn render_middle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let controls = self.render_controls(cx);
        let time = self.render_time(cx);
        let mut col = v_flex().gap_2().child(controls).child(time);
        let multi = self.out.as_ref().filter(|o| o.priced.multi_expiry).map(|o| o.priced.horizon_days);
        let (t_main, t_dens, t_dec) = match multi {
            Some(h) => (
                format!("最早到期日（{h:.0} 天）時的組合價值：目標 vs 複製 · 今日 / 半途價值（每單位）"),
                format!("最早到期日（{h:.0} 天）的價格分佈（每 1% 區間機率）· 風險中性 Q vs 真實世界 P"),
                format!("分解：每條線 = 一個部位在 {h:.0} 天時的價值（較晚到期者為 BS 價值曲線）"),
            ),
            None => (
                "Payoff：目標 f(S) vs 複製 g(S) · 今日 / 半途價值（每單位）".to_string(),
                "到期價格分佈（每 1% 價格區間的機率）· 風險中性 Q vs 真實世界 P".to_string(),
                "分解：函數 = 基本元件之和（每條線 = 一個部位的到期損益）".to_string(),
            ),
        };
        match &self.charts {
            Some(ch) => {
                col = col
                    .child(
                        panel_with(
                            if self.pnl_mode { format!("{t_main} · 顯示損益（= 價值 − 成本×e^rt）") } else { t_main },
                            h_flex()
                                .gap_1()
                                .child(chip("y-val", "價值", !self.pnl_mode, cx).on_click(cx.listener(|this, _, _, cx| {
                                    this.pnl_mode = false;
                                    this.rebuild_main();
                                    cx.notify();
                                })))
                                .child(chip("y-pnl", "損益", self.pnl_mode, cx).on_click(cx.listener(|this, _, _, cx| {
                                    this.pnl_mode = true;
                                    this.rebuild_main();
                                    cx.notify();
                                }))),
                            cx,
                        )
                        .h(px(360.0)).child(div().flex_1().min_h_0().p_1().child(xy_chart(ch.main.clone()))))
                    .child(panel(t_dens, cx).h(px(170.0)).child(div().flex_1().min_h_0().p_1().child(xy_chart(ch.density.clone()))))
                    .child(panel(t_dec, cx).h(px(230.0)).child(div().flex_1().min_h_0().p_1().child(xy_chart(ch.decomp.clone()))));
            }
            None => col = col.child(empty_state(if self.computing { "編譯中…" } else { "輸入函數後自動編譯" }, cx)),
        }
        col
    }

    fn stats_table(&self, q: &PayoffStats, pm: &PayoffStats, units: f64, cx: &mut Context<Self>) -> impl IntoElement {
        let cols = vec![Col::new("", 110.0).left(), Col::new("風險中性 Q", 104.0), Col::new("真實世界 P", 104.0)];
        let rows: Vec<(&str, String, String, Option<Hsla>)> = vec![
            ("期望 payoff", f2(q.mean), f2(pm.mean), None),
            ("期望損益 ×單位", money(q.expected_pnl * units), money(pm.expected_pnl * units), Some(sign_color(pm.expected_pnl))),
            ("獲利機率", pct(q.prob_profit), pct(pm.prob_profit), None),
            ("P5 payoff", f2(q.p05), f2(pm.p05), None),
            ("P50 payoff", f2(q.p50), f2(pm.p50), None),
            ("P95 payoff", f2(q.p95), f2(pm.p95), None),
        ];
        v_flex().child(table_header(&cols, cx)).children(rows.into_iter().enumerate().map(|(i, (l, a, b, c))| {
            table_row(SharedString::from(format!("qp-{i}")), &cols, vec![Cell::new(l).color(palette().muted), Cell::new(a), Cell::new(b).maybe_color(c)], i % 2 == 1, cx)
        }))
    }

    fn render_right(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let Some(o) = &self.out else {
            return v_flex().p_2().child(empty_state("—", cx)).into_any_element();
        };
        let rep = &o.rep;
        let pr = &o.priced;
        let u = pr.units;
        let g = pr.greeks;
        let cols = vec![
            Col::new("工具", 84.0).left(),
            Col::new("到期", 56.0),
            Col::new("數量/單位", 60.0),
            Col::new("口數", 46.0),
            Col::new("中價", 54.0),
            Col::new("成交側", 54.0),
            Col::new("IV", 44.0),
            Col::new("價值×單位", 66.0),
        ];
        // group legs by expiry
        let mut order: Vec<usize> = (0..pr.legs.len()).collect();
        order.sort_by(|a, b| pr.legs[*a].t.total_cmp(&pr.legs[*b].t));
        let legs = order.into_iter().enumerate().map(|(i, li)| {
            let l = &pr.legs[li];
            let exp_label = o.leg_exp.get(li).map(|e| e.0.clone()).unwrap_or_default();
            let is_opt = matches!(l.leg.instrument, Instrument::Call(_) | Instrument::Put(_));
            let side = if l.leg.qty >= 0.0 { l.ask } else { l.bid };
            let name = format!("{}{}", instrument_name(l.leg.instrument), if is_opt && !l.quoted && o.from_chain { " 模型價" } else { "" });
            table_row(
                SharedString::from(format!("pl-{i}")),
                &cols,
                vec![
                    Cell::new(name).color(match l.leg.instrument {
                        Instrument::Call(_) => p.green,
                        Instrument::Put(_) => p.red,
                        _ => p.muted,
                    }),
                    Cell::new(exp_label).color(p.cyan),
                    Cell::new(format!("{:+.3}", l.leg.qty)).color(sign_color(l.leg.qty)),
                    Cell::new(if is_opt { format!("{:+.2}", l.leg.qty * u / 100.0) } else { "—".into() }),
                    Cell::new(f2(l.mid)),
                    Cell::new(if is_opt { f2(side) } else { "—".into() }),
                    Cell::new(if is_opt { pct(l.iv) } else { "—".into() }).color(p.purple),
                    Cell::new(money(l.value * u)),
                ],
                i % 2 == 1,
                cx,
            )
        });
        let tw = table_width(&cols);
        let be = if pr.breakevens.is_empty() { "—".to_string() } else { pr.breakevens.iter().map(|b| format!("{b:.2}")).collect::<Vec<_>>().join(" / ") };
        let diff = pr.fair_target - pr.fair_replicated;
        v_flex()
            .gap_2()
            .p_2()
            .child(
                panel_with(
                    "編譯結果 g(S) = 現金 + 股票 + 買權 + 賣權",
                    h_flex()
                        .gap_1()
                        .child(chip("copy-f", "複製公式", false, cx).on_click(cx.listener(|this, _, _, cx| this.copy_formula(cx))))
                        .child(chip("csv-f", "匯出 CSV", false, cx).on_click(cx.listener(|this, _, _, cx| this.export_csv(cx))))
                        .child(chip("to-lab", "→ 策略實驗室", false, cx).on_click(cx.listener(|this, _, _, cx| this.send_to_strategy_lab(cx)))),
                    cx,
                )
                .child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .when(rep.groups.len() <= 1, |d| d.child(div().text_size(px(12.0)).font_family(MONO_FONT).text_color(p.text).child(rep.formula())))
                        .when(rep.groups.len() > 1, |d| {
                            d.child(muted_text(format!("{} 個到期日 · 分析時點 = 最早到期 {:.0} 天", rep.groups.len(), pr.horizon_days), cx)).children(rep.groups.iter().map(|g| {
                                v_flex()
                                    .gap_0p5()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .child(badge(format!("@ {:.0} 天", g.days), p.cyan))
                                            .child(badge(if g.approximated { "近似" } else { "精確" }, if g.approximated { p.yellow } else { p.green }))
                                            .child(muted_text(format!("誤差 最大 {:.4} · 平均 {:.4} · {} 部位", g.max_error, g.mean_error, g.n_legs), cx)),
                                    )
                                    .child(div().text_size(px(12.0)).font_family(MONO_FONT).text_color(p.text).child(format!("f(S) ≈ {}", g.formula)))
                            }))
                        })
                        .child(
                            h_flex()
                                .gap_2()
                                .child(badge(if rep.approximated { "近似" } else if !rep.snapped.is_empty() { "形狀精確・履約價取整" } else { "精確" }, if rep.approximated { p.yellow } else if !rep.snapped.is_empty() { p.blue } else { p.green }))
                                .child(muted_text(format!("複製誤差 最大 {:.4} · 平均 {:.4}（{:.0}–{:.0}）· {} 個部位 · {} 個折點", rep.max_error, rep.mean_error, pr.spot * 0.5, pr.spot * 1.6, rep.legs.len(), rep.kinks.len()), cx)),
                        ),
                ),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(kpi_sm("函數價格 中價/單位", f2(pr.price_mid), Some(if pr.price_mid > 0.0 { p.red } else { p.green }), cx))
                    .child(kpi_sm(format!("成本 ×{}", f0(u)), money(pr.price_mid * u), None, cx))
                    .child(kpi_sm("可成交成本", money(pr.price_exec * u), None, cx)),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(kpi_sm("公允值 目標 / 複製", format!("{} / {}", f2(pr.fair_target), f2(pr.fair_replicated)), None, cx))
                    .child(kpi_sm("近似/取整成本", format!("{diff:+.3}"), Some(if diff.abs() > 0.01 * pr.price_mid.abs().max(1.0) { p.yellow } else { p.muted }), cx))
                    .child(kpi_sm("損益兩平", be, Some(p.orange), cx)),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .child(kpi_sm("Δ 股 ×單位", f1(g.delta * u), None, cx))
                    .child(kpi_sm("Γ ×單位", f3(g.gamma * u), None, cx))
                    .child(kpi_sm("Θ $/日", f2(g.theta * u), Some(sign_color(g.theta)), cx))
                    .child(kpi_sm("Vega $/點", f2(g.vega * u), None, cx)),
            )
            .child(panel(format!("部位明細（每單位；口數 = 數量 × {} / 100）", f0(u)), cx).child(div().id("studio-legs").max_h(px(260.0)).overflow_y_scroll().child(v_flex().w(px(tw)).child(table_header(&cols, cx)).children(legs))))
            .child(panel(
                if pr.multi_expiry {
                    format!("Q vs P 分佈統計 · 最早到期日 {:.0} 天時（μ = {:.0}%）", pr.horizon_days, pr.mu * 100.0)
                } else {
                    format!("Q vs P 分佈統計（T = {:.2} 年，μ = {:.0}%）", pr.t, pr.mu * 100.0)
                },
                cx,
            ).child(self.stats_table(&pr.risk_neutral, &pr.real_world, u, cx)))
            .when(!rep.warnings.is_empty() || !rep.snapped.is_empty(), |d| {
                d.child(
                    panel("警告 Warnings", cx).child(
                        v_flex()
                            .p_2()
                            .gap_1()
                            .children(rep.warnings.iter().map(|w| div().text_size(px(11.5)).text_color(p.yellow).child(format!("⚠ {w}"))))
                            .when(!rep.snapped.is_empty(), |d| {
                                d.child(div().text_size(px(11.0)).text_color(p.muted).child(format!(
                                    "取整履約價：{}",
                                    rep.snapped.iter().take(8).map(|(a, b)| format!("{a:.2}→{b:.2}")).collect::<Vec<_>>().join(", ")
                                )))
                            }),
                    ),
                )
            })
            .into_any_element()
    }
}

impl Render for StudioPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette();
        let left = self.render_left(cx);
        let middle = self.render_middle(cx);
        let right = self.render_right(cx);
        h_flex()
            .size_full()
            .items_start()
            .child(div().id("studio-left").w(px(340.0)).flex_none().h_full().overflow_y_scroll().border_r_1().border_color(p.border).child(left))
            .child(div().id("studio-mid").flex_1().min_w_0().h_full().overflow_y_scroll().p_2().child(middle))
            .child(div().id("studio-right").w(px(484.0)).flex_none().h_full().overflow_y_scroll().border_l_1().border_color(p.border).child(right))
    }
}

#[allow(dead_code)]
fn _assert_send() {
    fn s<T: Send>() {}
    s::<Job>();
    s::<Out>();
}
