//! Small layout helpers in Zed's dense style: panels, KPI tiles, form fields, tables.

use gpui::{
    div, prelude::*, px, AnyElement, App, Div, Entity, Hsla, SharedString, Stateful, Window,
};
use gpui_component::{
    h_flex,
    input::{Input, InputState},
    v_flex, ActiveTheme, Sizable,
};

use crate::theme::{palette, MONO_FONT};

/// A bordered panel with a compact header line.
pub fn panel(title: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .border_1()
        .border_color(theme.border)
        .rounded(px(4.0))
        .bg(theme.background)
        .overflow_hidden()
        .child(
            h_flex()
                .h(px(26.0))
                .px_2()
                .flex_none()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .text_size(px(11.5))
                .text_color(theme.muted_foreground)
                .child(title.into()),
        )
}

/// Panel header with right-side extras.
pub fn panel_with(title: impl Into<SharedString>, extra: impl IntoElement, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .border_1()
        .border_color(theme.border)
        .rounded(px(4.0))
        .bg(theme.background)
        .overflow_hidden()
        .child(
            h_flex()
                .h(px(28.0))
                .px_2()
                .flex_none()
                .justify_between()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .text_size(px(11.5))
                .text_color(theme.muted_foreground)
                .child(title.into())
                .child(h_flex().gap_1().child(extra)),
        )
}

/// Body area of a panel.
pub fn panel_body() -> Div {
    v_flex().p_2().gap_2()
}

/// Big number tile.
pub fn kpi(label: impl Into<SharedString>, value: impl Into<SharedString>, color: Option<Hsla>, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .min_w(px(110.0))
        .flex_1()
        .px_2()
        .py_1p5()
        .gap_0p5()
        .border_1()
        .border_color(theme.border)
        .rounded(px(4.0))
        .bg(theme.sidebar)
        .child(div().text_size(px(11.0)).text_color(theme.muted_foreground).whitespace_nowrap().child(label.into()))
        .child(
            div()
                .font_family(MONO_FONT)
                .text_size(px(15.0))
                .whitespace_nowrap()
                .text_color(color.unwrap_or(theme.foreground))
                .child(value.into()),
        )
}

/// Label above an input.
pub fn field(label: impl Into<SharedString>, input: &Entity<InputState>, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .gap_0p5()
        .min_w(px(80.0))
        .child(div().text_size(px(11.0)).text_color(theme.muted_foreground).child(label.into()))
        .child(Input::new(input).small())
}

/// Label above an input, with an optional inline error under it.
pub fn field_err(
    label: impl Into<SharedString>,
    input: &Entity<InputState>,
    err: Option<&String>,
    cx: &App,
) -> Div {
    let p = palette();
    let mut f = field(label, input, cx);
    if let Some(e) = err {
        f = f.child(div().text_size(px(10.5)).text_color(p.red).child(e.clone()));
    }
    f
}

/// Label + arbitrary control.
pub fn labeled(label: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .gap_0p5()
        .child(div().text_size(px(11.0)).text_color(theme.muted_foreground).child(label.into()))
        .child(control)
}

/// Horizontal key: value line.
pub fn kv(key: impl Into<SharedString>, value: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .justify_between()
        .gap_2()
        .text_size(px(12.0))
        .child(div().text_color(theme.muted_foreground).child(key.into()))
        .child(div().font_family(MONO_FONT).child(value.into()))
}

pub fn kv_colored(key: impl Into<SharedString>, value: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .justify_between()
        .gap_2()
        .text_size(px(12.0))
        .child(div().text_color(theme.muted_foreground).child(key.into()))
        .child(div().font_family(MONO_FONT).text_color(color).child(value.into()))
}

pub fn muted_text(t: impl Into<SharedString>, cx: &App) -> Div {
    div().text_size(px(11.5)).text_color(cx.theme().muted_foreground).child(t.into())
}

pub fn section_title(t: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_size(px(12.0))
        .text_color(cx.theme().foreground)
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .child(t.into())
}

/// Small coloured badge.
pub fn badge(t: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .px_1p5()
        .py(px(1.0))
        .rounded(px(3.0))
        .text_size(px(10.5))
        .bg(color.opacity(0.18))
        .text_color(color)
        .child(t.into())
}

// ---------------------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum ColAlign {
    Left,
    Right,
    Center,
}

#[derive(Clone)]
pub struct Col {
    pub title: SharedString,
    pub width: f32,
    pub align: ColAlign,
}

impl Col {
    pub fn new(title: impl Into<SharedString>, width: f32) -> Self {
        Self { title: title.into(), width, align: ColAlign::Right }
    }
    pub fn left(mut self) -> Self {
        self.align = ColAlign::Left;
        self
    }
    pub fn center(mut self) -> Self {
        self.align = ColAlign::Center;
        self
    }
}

fn aligned(d: Div, a: ColAlign) -> Div {
    match a {
        ColAlign::Left => d.justify_start(),
        ColAlign::Right => d.justify_end(),
        ColAlign::Center => d.justify_center(),
    }
}

pub fn table_header(cols: &[Col], cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .h(px(24.0))
        .bg(theme.table_head)
        .border_b_1()
        .border_color(theme.border)
        .text_size(px(11.0))
        .text_color(theme.muted_foreground)
        .children(cols.iter().map(|c| {
            aligned(h_flex().w(px(c.width)).flex_none().px_1p5().overflow_hidden(), c.align)
                .child(c.title.clone())
        }))
}

/// A table cell value with optional colour.
pub struct Cell {
    pub text: SharedString,
    pub color: Option<Hsla>,
    pub bg: Option<Hsla>,
}

impl Cell {
    pub fn new(t: impl Into<SharedString>) -> Self {
        Self { text: t.into(), color: None, bg: None }
    }
    pub fn color(mut self, c: Hsla) -> Self {
        self.color = Some(c);
        self
    }
    pub fn maybe_color(mut self, c: Option<Hsla>) -> Self {
        self.color = c;
        self
    }
    pub fn bg(mut self, c: Hsla) -> Self {
        self.bg = Some(c);
        self
    }
}

impl From<String> for Cell {
    fn from(s: String) -> Self {
        Cell::new(s)
    }
}
impl From<&str> for Cell {
    fn from(s: &str) -> Self {
        Cell::new(s.to_string())
    }
}

pub fn table_row(id: impl Into<gpui::ElementId>, cols: &[Col], cells: Vec<Cell>, even: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let row_bg = if even { theme.table_even } else { theme.table };
    h_flex()
        .id(id)
        .flex_none()
        .h(px(22.0))
        .bg(row_bg)
        .border_b_1()
        .border_color(theme.table_row_border.opacity(0.5))
        .text_size(px(11.5))
        .font_family(MONO_FONT)
        .hover(|s| s.bg(theme.table_hover))
        .children(cols.iter().zip(cells).map(|(c, cell)| {
            let mut d = aligned(h_flex().h_full().w(px(c.width)).flex_none().px_1p5().overflow_hidden(), c.align);
            if let Some(col) = cell.color {
                d = d.text_color(col);
            }
            if let Some(b) = cell.bg {
                d = d.bg(b);
            }
            d.child(cell.text)
        }))
}

pub fn table_width(cols: &[Col]) -> f32 {
    cols.iter().map(|c| c.width).sum()
}

/// Empty-state placeholder.
pub fn empty_state(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_size(px(12.0))
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

// ---------------------------------------------------------------------------------------
// Number inputs
// ---------------------------------------------------------------------------------------

pub fn new_input(default: impl Into<SharedString>, window: &mut Window, cx: &mut App) -> Entity<InputState> {
    let d: SharedString = default.into();
    cx.new(|cx| InputState::new(window, cx).default_value(d))
}

pub fn new_input_ph(
    default: impl Into<SharedString>,
    placeholder: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    let d: SharedString = default.into();
    let p: SharedString = placeholder.into();
    cx.new(|cx| InputState::new(window, cx).default_value(d).placeholder(p))
}

pub fn input_text(e: &Entity<InputState>, cx: &App) -> String {
    e.read(cx).value().trim().to_string()
}

/// Parse a float; accepts "%" suffix (divides by 100 when `pct_ok`) and thousands commas.
pub fn parse_num(s: &str) -> Result<f64, String> {
    let t = s.trim().replace(',', "").replace('_', "");
    if t.is_empty() {
        return Err("必填".into());
    }
    let (body, pct) = match t.strip_suffix('%') {
        Some(b) => (b.trim().to_string(), true),
        None => (t.clone(), false),
    };
    match body.parse::<f64>() {
        Ok(v) if v.is_finite() => Ok(if pct { v / 100.0 } else { v }),
        _ => Err(format!("無法解析數字「{}」", s.trim())),
    }
}

/// Collects parse errors for a form.
#[derive(Default, Clone)]
pub struct FormErrors {
    pub errors: Vec<(&'static str, String)>,
}

impl FormErrors {
    pub fn num(&mut self, key: &'static str, e: &Entity<InputState>, cx: &App) -> f64 {
        match parse_num(&input_text(e, cx)) {
            Ok(v) => v,
            Err(err) => {
                self.errors.push((key, err));
                f64::NAN
            }
        }
    }
    pub fn num_range(&mut self, key: &'static str, e: &Entity<InputState>, lo: f64, hi: f64, cx: &App) -> f64 {
        let v = self.num(key, e, cx);
        if v.is_finite() && (v < lo || v > hi) {
            self.errors.push((key, format!("須介於 {lo} 與 {hi}")));
        }
        v
    }
    pub fn int(&mut self, key: &'static str, e: &Entity<InputState>, lo: i64, hi: i64, cx: &App) -> i64 {
        let v = self.num(key, e, cx);
        if !v.is_finite() {
            return lo;
        }
        let i = v.round() as i64;
        if i < lo || i > hi {
            self.errors.push((key, format!("須介於 {lo} 與 {hi}")));
        }
        i.clamp(lo, hi)
    }
    pub fn date(&mut self, key: &'static str, e: &Entity<InputState>, cx: &App) -> Option<chrono::NaiveDate> {
        let t = input_text(e, cx);
        if t.is_empty() {
            return None;
        }
        match chrono::NaiveDate::parse_from_str(&t, "%Y-%m-%d") {
            Ok(d) => Some(d),
            Err(_) => {
                self.errors.push((key, "日期格式須為 YYYY-MM-DD".into()));
                None
            }
        }
    }
    /// Parse a comma/space separated list of numbers, or "a:b:step" range.
    pub fn list(&mut self, key: &'static str, e: &Entity<InputState>, cx: &App) -> Vec<f64> {
        let t = input_text(e, cx);
        match parse_list(&t) {
            Ok(v) if !v.is_empty() => v,
            Ok(_) => {
                self.errors.push((key, "至少需要一個值".into()));
                vec![]
            }
            Err(err) => {
                self.errors.push((key, err));
                vec![]
            }
        }
    }
    pub fn get(&self, key: &str) -> Option<&String> {
        self.errors.iter().find(|(k, _)| *k == key).map(|(_, e)| e)
    }
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
    pub fn summary(&self) -> String {
        self.errors.iter().map(|(k, e)| format!("{k}: {e}")).collect::<Vec<_>>().join("；")
    }
}

pub fn parse_list(t: &str) -> Result<Vec<f64>, String> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(vec![]);
    }
    if t.contains(':') {
        let parts: Vec<&str> = t.split(':').collect();
        if parts.len() != 3 {
            return Err("範圍格式為 起:迄:步長".into());
        }
        let a = parse_num(parts[0])?;
        let b = parse_num(parts[1])?;
        let s = parse_num(parts[2])?;
        if s <= 0.0 || b < a {
            return Err("步長須 > 0 且 迄 ≥ 起".into());
        }
        let n = ((b - a) / s + 1e-9).floor() as usize + 1;
        if n > 200 {
            return Err("範圍內值過多 (>200)".into());
        }
        return Ok((0..n).map(|i| a + s * i as f64).collect());
    }
    t.split(|c: char| c == ',' || c == ' ' || c == '，' || c == ';')
        .filter(|s| !s.trim().is_empty())
        .map(parse_num)
        .collect()
}

/// Convert any element-producing closure output into AnyElement (helper for match arms).
pub fn any(e: impl IntoElement) -> AnyElement {
    e.into_any_element()
}

// ---------------------------------------------------------------------------------------
// Number formatting
// ---------------------------------------------------------------------------------------

pub fn f2(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.2}")
    } else {
        "—".into()
    }
}

pub fn f1(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.1}")
    } else {
        "—".into()
    }
}

pub fn f0(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.0}")
    } else {
        "—".into()
    }
}

pub fn f3(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.3}")
    } else {
        "—".into()
    }
}

pub fn pct(v: f64) -> String {
    if v.is_finite() {
        format!("{:.1}%", v * 100.0)
    } else {
        "—".into()
    }
}

pub fn pct2(v: f64) -> String {
    if v.is_finite() {
        format!("{:.2}%", v * 100.0)
    } else {
        "—".into()
    }
}

pub fn opt_f2(v: Option<f64>) -> String {
    v.map(f2).unwrap_or_else(|| "—".into())
}

pub fn opt_pct(v: Option<f64>) -> String {
    v.map(pct).unwrap_or_else(|| "—".into())
}

pub fn money(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let neg = v < 0.0;
    let a = v.abs().round() as i64;
    let s = a.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if neg {
        format!("-${out}")
    } else {
        format!("${out}")
    }
}

/// Green for positive, red for negative.
pub fn sign_color(v: f64) -> Hsla {
    let p = palette();
    if v > 0.0 {
        p.green
    } else if v < 0.0 {
        p.red
    } else {
        p.text
    }
}

/// Compact toggle chip (segmented-control item). Attach `.on_click(...)` at the call site.
pub fn chip(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, active: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let p = palette();
    h_flex()
        .id(id)
        .h(px(22.0))
        .px_2()
        .rounded(px(3.0))
        .cursor_pointer()
        .text_size(px(11.5))
        .border_1()
        .when(active, |d| d.bg(p.blue.opacity(0.2)).border_color(p.blue.opacity(0.6)).text_color(p.text))
        .when(!active, |d| d.border_color(theme.border).text_color(p.muted).hover(|s| s.bg(p.elevated)))
        .child(label.into())
}

/// Level colour for advice cards.
pub fn advice_color(level: optlab_core::advisor::AdviceLevel) -> Hsla {
    use optlab_core::advisor::AdviceLevel::*;
    let p = palette();
    match level {
        Info => p.blue,
        Suggest => p.green,
        Warning => p.yellow,
        Risk => p.red,
    }
}

/// Advice card (title + body) with a coloured left border.
pub fn advice_card(a: &optlab_core::advisor::Advice, cx: &App) -> Div {
    let theme = cx.theme();
    let c = advice_color(a.level);
    h_flex()
        .items_start()
        .border_1()
        .border_color(theme.border)
        .rounded(px(4.0))
        .bg(theme.sidebar)
        .overflow_hidden()
        .child(div().w(px(3.0)).flex_none().h_full().min_h(px(40.0)).bg(c))
        .child(
            v_flex()
                .p_2()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .gap_2()
                        .child(badge(a.level.label(), c))
                        .child(div().text_size(px(12.5)).font_weight(gpui::FontWeight::SEMIBOLD).child(a.title.clone())),
                )
                .child(div().text_size(px(12.0)).text_color(theme.muted_foreground).child(a.body.clone())),
        )
}
