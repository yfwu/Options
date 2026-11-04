//! Lightweight, dependency-free chart elements painted with gpui `canvas()`.
//!
//! * [`XyChart`]: multi-series line / area / bar / scatter chart with numeric or date
//!   x-axis, optional log-scale y, reference lines, legend and hover crosshair readout.
//! * [`Heatmap`]: labelled matrix with a diverging colour scale and hover readout.
//!
//! Charts take their data behind an `Rc` so that re-rendering (which happens on every
//! hover frame) is cheap.

use std::rc::Rc;

use chrono::{Datelike, NaiveDate};
use gpui::{
    canvas, div, fill, point, prelude::*, px, size, App, Bounds, ContentMask, DispatchPhase,
    Hsla, MouseMoveEvent, PathBuilder, Pixels, Point, SharedString, TextRun, Window,
};
use gpui_component::ActiveTheme;

use crate::theme::palette;

// ---------------------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeriesKind {
    Line,
    /// Line with the area down (or up) to `baseline` filled.
    Area { baseline: f64 },
    /// Vertical bars centred on x with the given width in x units.
    Bars { width: f64 },
    /// Dots.
    Points { radius: f32 },
}

#[derive(Clone, Debug)]
pub struct Series {
    pub name: SharedString,
    /// (x, y) sorted by x ascending.
    pub points: Vec<(f64, f64)>,
    pub color: Hsla,
    pub kind: SeriesKind,
    pub width: f32,
    pub dashed: bool,
    /// Optional per-point colours (bars / points).
    pub point_colors: Option<Vec<Hsla>>,
    pub legend: bool,
}

impl Series {
    pub fn line(name: impl Into<SharedString>, points: Vec<(f64, f64)>, color: Hsla) -> Self {
        Self {
            name: name.into(),
            points,
            color,
            kind: SeriesKind::Line,
            width: 1.5,
            dashed: false,
            point_colors: None,
            legend: true,
        }
    }
    pub fn area(
        name: impl Into<SharedString>,
        points: Vec<(f64, f64)>,
        color: Hsla,
        baseline: f64,
    ) -> Self {
        Self { kind: SeriesKind::Area { baseline }, ..Self::line(name, points, color) }
    }
    pub fn bars(
        name: impl Into<SharedString>,
        points: Vec<(f64, f64)>,
        color: Hsla,
        width: f64,
    ) -> Self {
        Self { kind: SeriesKind::Bars { width }, ..Self::line(name, points, color) }
    }
    pub fn points(name: impl Into<SharedString>, points: Vec<(f64, f64)>, color: Hsla) -> Self {
        Self { kind: SeriesKind::Points { radius: 3.0 }, ..Self::line(name, points, color) }
    }
    pub fn dashed(mut self) -> Self {
        self.dashed = true;
        self
    }
    pub fn width(mut self, w: f32) -> Self {
        self.width = w;
        self
    }
    pub fn colors(mut self, c: Vec<Hsla>) -> Self {
        self.point_colors = Some(c);
        self
    }
    pub fn no_legend(mut self) -> Self {
        self.legend = false;
        self
    }
    /// Colour bars green when y >= 0 and red otherwise.
    pub fn signed_colors(mut self) -> Self {
        let p = palette();
        self.point_colors =
            Some(self.points.iter().map(|(_, y)| if *y >= 0.0 { p.green } else { p.red }).collect());
        self
    }
}

#[derive(Clone, Debug)]
pub struct RefLine {
    pub value: f64,
    pub color: Hsla,
    pub label: Option<SharedString>,
    pub dashed: bool,
}

impl RefLine {
    pub fn new(value: f64, color: Hsla) -> Self {
        Self { value, color, label: None, dashed: true }
    }
    pub fn label(mut self, l: impl Into<SharedString>) -> Self {
        self.label = Some(l.into());
        self
    }
    pub fn solid(mut self) -> Self {
        self.dashed = false;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XKind {
    Number,
    /// x values are days since CE (`NaiveDate::num_days_from_ce`).
    Date,
}

pub type Fmt = fn(f64) -> String;

#[derive(Clone)]
pub struct ChartSpec {
    pub series: Vec<Series>,
    pub x_kind: XKind,
    pub x_fmt: Fmt,
    pub y_fmt: Fmt,
    pub log_y: bool,
    pub hlines: Vec<RefLine>,
    pub vlines: Vec<RefLine>,
    pub y_min: Option<f64>,
    pub y_max: Option<f64>,
    pub include_zero: bool,
    pub legend: bool,
    pub empty_text: SharedString,
}

impl Default for ChartSpec {
    fn default() -> Self {
        Self {
            series: vec![],
            x_kind: XKind::Number,
            x_fmt: fmt_auto,
            y_fmt: fmt_auto,
            log_y: false,
            hlines: vec![],
            vlines: vec![],
            y_min: None,
            y_max: None,
            include_zero: false,
            legend: true,
            empty_text: "無資料".into(),
        }
    }
}

impl ChartSpec {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn dates(mut self) -> Self {
        self.x_kind = XKind::Date;
        self.x_fmt = fmt_date;
        self
    }
    pub fn series(mut self, s: Series) -> Self {
        self.series.push(s);
        self
    }
    pub fn x_fmt(mut self, f: Fmt) -> Self {
        self.x_fmt = f;
        self
    }
    pub fn y_fmt(mut self, f: Fmt) -> Self {
        self.y_fmt = f;
        self
    }
    pub fn log_y(mut self, on: bool) -> Self {
        self.log_y = on;
        self
    }
    pub fn hline(mut self, l: RefLine) -> Self {
        self.hlines.push(l);
        self
    }
    pub fn vline(mut self, l: RefLine) -> Self {
        self.vlines.push(l);
        self
    }
    pub fn include_zero(mut self) -> Self {
        self.include_zero = true;
        self
    }
    pub fn y_range(mut self, lo: Option<f64>, hi: Option<f64>) -> Self {
        self.y_min = lo;
        self.y_max = hi;
        self
    }
    pub fn no_legend(mut self) -> Self {
        self.legend = false;
        self
    }
    pub fn empty_text(mut self, t: impl Into<SharedString>) -> Self {
        self.empty_text = t.into();
        self
    }
    pub fn into_rc(self) -> Rc<ChartSpec> {
        Rc::new(self)
    }
}

// ---------------------------------------------------------------------------------------
// Formatters
// ---------------------------------------------------------------------------------------

pub fn date_to_x(d: NaiveDate) -> f64 {
    d.num_days_from_ce() as f64
}

pub fn x_to_date(x: f64) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(x.round() as i32)
}

pub fn fmt_date(x: f64) -> String {
    x_to_date(x).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

pub fn fmt_auto(v: f64) -> String {
    let a = v.abs();
    if !v.is_finite() {
        "—".into()
    } else if a >= 1e9 {
        format!("{:.2}B", v / 1e9)
    } else if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}K", v / 1e3)
    } else if a >= 100.0 {
        format!("{:.0}", v)
    } else if a >= 10.0 {
        format!("{:.1}", v)
    } else if a >= 1.0 {
        format!("{:.2}", v)
    } else if a == 0.0 {
        "0".into()
    } else {
        format!("{:.3}", v)
    }
}

pub fn fmt_int(v: f64) -> String {
    format!("{:.0}", v)
}

pub fn fmt_pct(v: f64) -> String {
    format!("{:.0}%", v * 100.0)
}

pub fn fmt_pct1(v: f64) -> String {
    format!("{:.1}%", v * 100.0)
}

pub fn fmt_money(v: f64) -> String {
    let a = v.abs();
    let s = if a >= 1e6 {
        format!("{:.2}M", a / 1e6)
    } else if a >= 1e4 {
        format!("{:.1}K", a / 1e3)
    } else {
        format!("{:.0}", a)
    };
    if v < 0.0 {
        format!("-${s}")
    } else {
        format!("${s}")
    }
}

// ---------------------------------------------------------------------------------------
// Axis helpers
// ---------------------------------------------------------------------------------------

fn nice_step(range: f64, target: usize) -> f64 {
    let raw = range / target.max(1) as f64;
    if raw <= 0.0 || !raw.is_finite() {
        return 1.0;
    }
    let mag = 10f64.powf(raw.log10().floor());
    let norm = raw / mag;
    let n = if norm < 1.5 {
        1.0
    } else if norm < 3.0 {
        2.0
    } else if norm < 7.0 {
        5.0
    } else {
        10.0
    };
    n * mag
}

fn linear_ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    let step = nice_step(hi - lo, target);
    let mut v = (lo / step).ceil() * step;
    let mut out = vec![];
    while v <= hi + step * 1e-9 && out.len() < 50 {
        out.push(if v.abs() < step * 1e-9 { 0.0 } else { v });
        v += step;
    }
    out
}

fn log_ticks(lo: f64, hi: f64) -> Vec<f64> {
    // lo/hi are raw (positive) values
    let mut out = vec![];
    let e0 = lo.log10().floor() as i32;
    let e1 = hi.log10().ceil() as i32;
    let decades = (e1 - e0).max(1);
    let mults: &[f64] = if decades <= 1 {
        &[1.0, 1.5, 2.0, 3.0, 5.0, 7.0]
    } else if decades <= 3 {
        &[1.0, 2.0, 5.0]
    } else {
        &[1.0]
    };
    for e in e0..=e1 {
        for m in mults {
            let v = m * 10f64.powi(e);
            if v >= lo && v <= hi {
                out.push(v);
            }
        }
    }
    if out.len() < 2 {
        return linear_ticks(lo, hi, 4);
    }
    out
}

/// Ticks for a date axis; returns (x, label).
fn date_ticks(lo: f64, hi: f64, max_ticks: usize) -> Vec<(f64, String)> {
    let (Some(d0), Some(d1)) = (x_to_date(lo), x_to_date(hi)) else {
        return vec![];
    };
    let days = (hi - lo).max(1.0);
    let max_ticks = max_ticks.max(2);
    let mut out = vec![];
    if days > 365.0 * 2.5 {
        let years = days / 365.25;
        let step = [1, 2, 5, 10, 20]
            .into_iter()
            .find(|s| years / *s as f64 <= max_ticks as f64)
            .unwrap_or(25);
        let mut y = d0.year() + 1;
        while y % step != 0 {
            y += 1;
        }
        while y <= d1.year() {
            if let Some(d) = NaiveDate::from_ymd_opt(y, 1, 1) {
                out.push((date_to_x(d), format!("{y}")));
            }
            y += step;
        }
    } else if days > 45.0 {
        let months = days / 30.4;
        let step = [1, 2, 3, 6, 12]
            .into_iter()
            .find(|s| months / *s as f64 <= max_ticks as f64)
            .unwrap_or(12);
        let mut y = d0.year();
        let mut m = d0.month() as i32 + 1;
        loop {
            if m > 12 {
                m -= 12;
                y += 1;
            }
            let Some(d) = NaiveDate::from_ymd_opt(y, m as u32, 1) else { break };
            if d > d1 {
                break;
            }
            if (m - 1) % step == 0 {
                out.push((date_to_x(d), d.format("%Y-%m").to_string()));
            }
            m += 1;
        }
    } else {
        let step = ((days / max_ticks as f64).ceil() as i64).max(1);
        let mut d = d0;
        while d <= d1 {
            out.push((date_to_x(d), d.format("%m-%d").to_string()));
            d += chrono::Duration::days(step);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------
// Painting helpers
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Align {
    Left,
    Center,
    Right,
}

fn text_run(len: usize, color: Hsla, mono: bool, window: &Window, cx: &App) -> TextRun {
    let mut font = window.text_style().font();
    if mono {
        font.family = cx.theme().mono_font_family.clone();
    }
    TextRun { len, font, color, background_color: None, underline: None, strikethrough: None }
}

/// Measure width of a single line of text.
pub fn text_width(text: &str, size_px: f32, mono: bool, window: &Window, cx: &App) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let run = text_run(text.len(), Hsla::default(), mono, window, cx);
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size_px),
        &[run],
        None,
    );
    f32::from(line.width)
}

/// Paint one line of text; `origin.y` is the top of the line box. Returns width.
#[allow(clippy::too_many_arguments)]
pub fn paint_text(
    text: &str,
    origin: Point<Pixels>,
    size_px: f32,
    color: Hsla,
    align: Align,
    mono: bool,
    window: &mut Window,
    cx: &mut App,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let run = text_run(text.len(), color, mono, window, cx);
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size_px),
        &[run],
        None,
    );
    let w = line.width;
    let o = match align {
        Align::Left => origin,
        Align::Center => point(origin.x - w / 2.0, origin.y),
        Align::Right => point(origin.x - w, origin.y),
    };
    let _ = line.paint(o, px(size_px * 1.25), window, cx);
    f32::from(w)
}

fn hline_quad(x0: f32, x1: f32, y: f32, color: Hsla, window: &mut Window) {
    window.paint_quad(fill(
        Bounds::new(point(px(x0), px(y.floor())), size(px((x1 - x0).max(0.0)), px(1.0))),
        color,
    ));
}

fn vline_quad(x: f32, y0: f32, y1: f32, color: Hsla, window: &mut Window) {
    window.paint_quad(fill(
        Bounds::new(point(px(x.floor()), px(y0)), size(px(1.0), px((y1 - y0).max(0.0)))),
        color,
    ));
}

fn stroke_polyline(pts: &[Point<Pixels>], width: f32, dashed: bool, color: Hsla, window: &mut Window) {
    if pts.len() < 2 {
        return;
    }
    let mut b = PathBuilder::stroke(px(width));
    if dashed {
        b = b.dash_array(&[px(5.0), px(3.0)]);
    }
    b.move_to(pts[0]);
    for p in &pts[1..] {
        b.line_to(*p);
    }
    if let Ok(path) = b.build() {
        window.paint_path(path, color);
    }
}

fn dashed_h(x0: f32, x1: f32, y: f32, color: Hsla, window: &mut Window) {
    stroke_polyline(&[point(px(x0), px(y)), point(px(x1), px(y))], 1.0, true, color, window);
}

fn dashed_v(x: f32, y0: f32, y1: f32, color: Hsla, window: &mut Window) {
    stroke_polyline(&[point(px(x), px(y0)), point(px(x), px(y1))], 1.0, true, color, window);
}

/// Decimate a polyline to at most ~2 points per pixel column (min/max preserving).
fn decimate(pts: Vec<Point<Pixels>>, width_px: f32) -> Vec<Point<Pixels>> {
    let max = (width_px as usize).max(10) * 2;
    if pts.len() <= max * 2 {
        return pts;
    }
    let mut out = Vec::with_capacity(max * 2 + 2);
    let mut cur_col = f32::NAN;
    let mut lo = pts[0];
    let mut hi = pts[0];
    let mut first = pts[0];
    for p in pts.iter().copied() {
        let col = f32::from(p.x).floor();
        if col != cur_col {
            if !cur_col.is_nan() {
                out.push(first);
                if lo.y < hi.y {
                    out.push(lo);
                    out.push(hi);
                } else {
                    out.push(hi);
                    out.push(lo);
                }
            }
            cur_col = col;
            lo = p;
            hi = p;
            first = p;
        } else {
            if p.y < lo.y {
                lo = p;
            }
            if p.y > hi.y {
                hi = p;
            }
        }
    }
    out.push(first);
    out.push(lo);
    out.push(hi);
    if let Some(l) = pts.last() {
        out.push(*l);
    }
    out
}

fn nearest_index(points: &[(f64, f64)], x: f64) -> Option<usize> {
    if points.is_empty() {
        return None;
    }
    let idx = points.partition_point(|p| p.0 < x);
    if idx == 0 {
        Some(0)
    } else if idx >= points.len() {
        Some(points.len() - 1)
    } else {
        let a = idx - 1;
        if (points[a].0 - x).abs() <= (points[idx].0 - x).abs() {
            Some(a)
        } else {
            Some(idx)
        }
    }
}

/// Draws a tooltip box with rows of (color, text).
fn paint_tooltip(
    anchor: Point<Pixels>,
    area: Bounds<Pixels>,
    title: &str,
    rows: &[(Hsla, String)],
    window: &mut Window,
    cx: &mut App,
) {
    let fs = 11.0;
    let lh = 15.0;
    let mut w = text_width(title, fs, true, window, cx);
    for (_, r) in rows {
        w = w.max(text_width(r, fs, true, window, cx) + 12.0);
    }
    let bw = w + 14.0;
    let bh = lh * (rows.len() as f32 + 1.0) + 8.0;
    let ax = f32::from(anchor.x);
    let ay = f32::from(anchor.y);
    let right = f32::from(area.origin.x + area.size.width);
    let bottom = f32::from(area.origin.y + area.size.height);
    let mut x = ax + 12.0;
    if x + bw > right {
        x = ax - 12.0 - bw;
    }
    let mut y = ay + 12.0;
    if y + bh > bottom {
        y = (bottom - bh).max(f32::from(area.origin.y));
    }
    let theme = cx.theme();
    let bg = theme.popover;
    let border = theme.border;
    let fg = theme.popover_foreground;
    let muted = theme.muted_foreground;
    let b = Bounds::new(point(px(x), px(y)), size(px(bw), px(bh)));
    window.paint_quad(gpui::quad(
        b,
        px(4.0),
        bg.opacity(0.96),
        px(1.0),
        border,
        gpui::BorderStyle::Solid,
    ));
    paint_text(title, point(px(x + 7.0), px(y + 4.0)), fs, muted, Align::Left, true, window, cx);
    for (i, (c, r)) in rows.iter().enumerate() {
        let ry = y + 4.0 + lh * (i as f32 + 1.0);
        window.paint_quad(fill(
            Bounds::new(point(px(x + 7.0), px(ry + 4.0)), size(px(7.0), px(7.0))),
            *c,
        ));
        paint_text(r, point(px(x + 19.0), px(ry)), fs, fg, Align::Left, true, window, cx);
    }
}

/// Requests a repaint whenever the mouse moves over (or leaves) `bounds`.
fn track_hover(bounds: Bounds<Pixels>, was_inside: bool, window: &mut Window) {
    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, _cx| {
        if phase == DispatchPhase::Bubble {
            let inside = bounds.contains(&ev.position);
            if inside || was_inside {
                window.refresh();
            }
        }
    });
}

// ---------------------------------------------------------------------------------------
// XY chart
// ---------------------------------------------------------------------------------------

/// Build an XY chart element that fills its parent.
pub fn xy_chart(spec: Rc<ChartSpec>) -> impl IntoElement {
    div().size_full().min_h(px(80.0)).child(
        canvas(|_, _, _| (), move |bounds, _, window, cx| paint_xy(&spec, bounds, window, cx))
            .size_full(),
    )
}

struct Mapper {
    plot: Bounds<Pixels>,
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
    log_y: bool,
}

impl Mapper {
    fn tx(&self, x: f64) -> f32 {
        let l = f32::from(self.plot.origin.x);
        let w = f32::from(self.plot.size.width);
        l + ((x - self.x0) / (self.x1 - self.x0)) as f32 * w
    }
    fn ty(&self, y: f64) -> f32 {
        let t = f32::from(self.plot.origin.y);
        let h = f32::from(self.plot.size.height);
        let v = if self.log_y { y.max(1e-12).ln() } else { y };
        t + h - ((v - self.y0) / (self.y1 - self.y0)) as f32 * h
    }
    fn inv_x(&self, px_x: f32) -> f64 {
        let l = f32::from(self.plot.origin.x);
        let w = f32::from(self.plot.size.width);
        self.x0 + ((px_x - l) / w) as f64 * (self.x1 - self.x0)
    }
    fn inv_y(&self, px_y: f32) -> f64 {
        let t = f32::from(self.plot.origin.y);
        let h = f32::from(self.plot.size.height);
        let v = self.y0 + ((t + h - px_y) / h) as f64 * (self.y1 - self.y0);
        if self.log_y {
            v.exp()
        } else {
            v
        }
    }
}

fn paint_xy(spec: &ChartSpec, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let grid = theme.border.opacity(0.45);
    let axis_c = theme.border;
    let fg = theme.foreground;

    // ---- data ranges
    let mut xmin = f64::INFINITY;
    let mut xmax = f64::NEG_INFINITY;
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for s in &spec.series {
        let half = match s.kind {
            SeriesKind::Bars { width } => width / 2.0,
            _ => 0.0,
        };
        for &(x, y) in &s.points {
            if !x.is_finite() || !y.is_finite() || (spec.log_y && y <= 0.0) {
                continue;
            }
            xmin = xmin.min(x - half);
            xmax = xmax.max(x + half);
            ymin = ymin.min(y);
            ymax = ymax.max(y);
        }
        if let SeriesKind::Area { baseline } = s.kind {
            if !spec.log_y {
                ymin = ymin.min(baseline);
                ymax = ymax.max(baseline);
            }
        }
        if matches!(s.kind, SeriesKind::Bars { .. }) && !spec.log_y {
            ymin = ymin.min(0.0);
            ymax = ymax.max(0.0);
        }
    }
    if !xmin.is_finite() || !ymin.is_finite() {
        paint_text(
            &spec.empty_text,
            point(bounds.center().x, bounds.center().y - px(8.0)),
            12.0,
            muted,
            Align::Center,
            false,
            window,
            cx,
        );
        return;
    }
    for l in &spec.hlines {
        if !spec.log_y || l.value > 0.0 {
            ymin = ymin.min(l.value);
            ymax = ymax.max(l.value);
        }
    }
    if spec.include_zero && !spec.log_y {
        ymin = ymin.min(0.0);
        ymax = ymax.max(0.0);
    }
    if let Some(v) = spec.y_min {
        ymin = v;
    }
    if let Some(v) = spec.y_max {
        ymax = v;
    }
    if xmax <= xmin {
        xmin -= 1.0;
        xmax += 1.0;
    }
    let (mut y0, mut y1) = if spec.log_y { (ymin.ln(), ymax.ln()) } else { (ymin, ymax) };
    if y1 <= y0 {
        let d = if y0.abs() > 1e-9 { y0.abs() * 0.05 } else { 1.0 };
        y0 -= d;
        y1 += d;
    }
    let pad = (y1 - y0) * 0.06;
    if spec.y_min.is_none() {
        y0 -= pad;
    }
    if spec.y_max.is_none() {
        y1 += pad;
    }

    // ---- y ticks & left margin
    let (raw_lo, raw_hi) = if spec.log_y { (y0.exp(), y1.exp()) } else { (y0, y1) };
    let plot_h_est = f32::from(bounds.size.height) - 30.0;
    let y_target = ((plot_h_est / 40.0) as usize).clamp(2, 8);
    let yt = if spec.log_y { log_ticks(raw_lo, raw_hi) } else { linear_ticks(raw_lo, raw_hi, y_target) };
    let y_labels: Vec<String> = yt.iter().map(|v| (spec.y_fmt)(*v)).collect();
    let fs = 10.5;
    let lw = y_labels
        .iter()
        .map(|l| text_width(l, fs, true, window, cx))
        .fold(0.0f32, f32::max);
    let left = lw + 10.0;
    let right = 12.0;
    let top = 8.0;
    let bottom = 20.0;
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let bw = f32::from(bounds.size.width);
    let bh = f32::from(bounds.size.height);
    if bw < left + right + 20.0 || bh < top + bottom + 20.0 {
        return;
    }
    let plot = Bounds::new(
        point(px(ox + left), px(oy + top)),
        size(px(bw - left - right), px(bh - top - bottom)),
    );
    let m = Mapper { plot, x0: xmin, x1: xmax, y0, y1, log_y: spec.log_y };
    let pl = f32::from(plot.origin.x);
    let pr = pl + f32::from(plot.size.width);
    let pt = f32::from(plot.origin.y);
    let pb = pt + f32::from(plot.size.height);

    // ---- grid + y labels
    for (v, label) in yt.iter().zip(y_labels.iter()) {
        let y = m.ty(*v);
        if y < pt - 0.5 || y > pb + 0.5 {
            continue;
        }
        hline_quad(pl, pr, y, grid, window);
        paint_text(label, point(px(pl - 5.0), px(y - 7.0)), fs, muted, Align::Right, true, window, cx);
    }
    // zero line emphasised
    if !spec.log_y && y0 < 0.0 && y1 > 0.0 {
        hline_quad(pl, pr, m.ty(0.0), axis_c, window);
    }

    // ---- x ticks
    let plot_w = pr - pl;
    let x_target = ((plot_w / 90.0) as usize).clamp(2, 12);
    let xt: Vec<(f64, String)> = match spec.x_kind {
        XKind::Date => date_ticks(xmin, xmax, x_target),
        XKind::Number => linear_ticks(xmin, xmax, x_target)
            .into_iter()
            .map(|v| (v, (spec.x_fmt)(v)))
            .collect(),
    };
    for (v, label) in &xt {
        let x = m.tx(*v);
        if x < pl - 0.5 || x > pr + 0.5 {
            continue;
        }
        vline_quad(x, pt, pb, grid.opacity(0.25), window);
        paint_text(label, point(px(x), px(pb + 4.0)), fs, muted, Align::Center, true, window, cx);
    }
    // axes
    hline_quad(pl, pr, pb, axis_c, window);
    vline_quad(pl, pt, pb, axis_c, window);

    // ---- series (clipped)
    let mask = ContentMask { bounds: plot };
    window.with_content_mask(Some(mask), |window| {
        for s in &spec.series {
            let pts: Vec<(f64, f64)> = s
                .points
                .iter()
                .copied()
                .filter(|(x, y)| x.is_finite() && y.is_finite() && (!spec.log_y || *y > 0.0))
                .collect();
            match s.kind {
                SeriesKind::Line | SeriesKind::Area { .. } => {
                    let poly: Vec<Point<Pixels>> =
                        pts.iter().map(|(x, y)| point(px(m.tx(*x)), px(m.ty(*y)))).collect();
                    let poly = decimate(poly, plot_w);
                    if let SeriesKind::Area { baseline } = s.kind {
                        if poly.len() >= 2 {
                            let by = if spec.log_y { pb } else { m.ty(baseline).clamp(pt, pb) };
                            let mut b = PathBuilder::fill();
                            b.move_to(point(poly[0].x, px(by)));
                            for p in &poly {
                                b.line_to(*p);
                            }
                            b.line_to(point(poly[poly.len() - 1].x, px(by)));
                            b.close();
                            if let Ok(path) = b.build() {
                                window.paint_path(path, s.color.opacity(0.22));
                            }
                        }
                    }
                    stroke_polyline(&poly, s.width, s.dashed, s.color, window);
                }
                SeriesKind::Bars { width } => {
                    let base = if spec.log_y { pb } else { m.ty(0.0).clamp(pt, pb) };
                    for (i, (x, y)) in pts.iter().enumerate() {
                        let xa = m.tx(x - width / 2.0);
                        let xb = m.tx(x + width / 2.0);
                        let w = (xb - xa - 1.0).max(1.0);
                        let yy = m.ty(*y);
                        let (top_y, h) = if yy < base { (yy, base - yy) } else { (base, yy - base) };
                        let c = s
                            .point_colors
                            .as_ref()
                            .and_then(|c| c.get(i).copied())
                            .unwrap_or(s.color);
                        window.paint_quad(fill(
                            Bounds::new(point(px(xa + 0.5), px(top_y)), size(px(w), px(h.max(1.0)))),
                            c.opacity(0.85),
                        ));
                    }
                }
                SeriesKind::Points { radius } => {
                    for (i, (x, y)) in pts.iter().enumerate() {
                        let c = s
                            .point_colors
                            .as_ref()
                            .and_then(|c| c.get(i).copied())
                            .unwrap_or(s.color);
                        let cx_ = m.tx(*x);
                        let cy_ = m.ty(*y);
                        window.paint_quad(gpui::quad(
                            Bounds::new(
                                point(px(cx_ - radius), px(cy_ - radius)),
                                size(px(radius * 2.0), px(radius * 2.0)),
                            ),
                            px(radius),
                            c,
                            px(0.0),
                            c,
                            gpui::BorderStyle::Solid,
                        ));
                    }
                }
            }
        }
        // reference lines
        for l in &spec.hlines {
            if spec.log_y && l.value <= 0.0 {
                continue;
            }
            let y = m.ty(l.value);
            if l.dashed {
                dashed_h(pl, pr, y, l.color, window);
            } else {
                hline_quad(pl, pr, y, l.color, window);
            }
        }
        for l in &spec.vlines {
            let x = m.tx(l.value);
            if l.dashed {
                dashed_v(x, pt, pb, l.color, window);
            } else {
                vline_quad(x, pt, pb, l.color, window);
            }
        }
    });
    // ref line labels (outside the clip so they stay legible)
    for l in &spec.hlines {
        if let Some(lbl) = &l.label {
            if spec.log_y && l.value <= 0.0 {
                continue;
            }
            let y = m.ty(l.value);
            if y >= pt && y <= pb {
                paint_text(lbl, point(px(pr - 4.0), px(y - 13.0)), 10.0, l.color, Align::Right, false, window, cx);
            }
        }
    }
    for l in &spec.vlines {
        if let Some(lbl) = &l.label {
            let x = m.tx(l.value);
            if x >= pl && x <= pr {
                paint_text(lbl, point(px(x + 3.0), px(pt + 2.0)), 10.0, l.color, Align::Left, false, window, cx);
            }
        }
    }

    // ---- legend
    if spec.legend {
        let mut lx = pl + 8.0;
        let ly = pt + 4.0;
        let legend_items: Vec<&Series> = spec.series.iter().filter(|s| s.legend).collect();
        if legend_items.len() > 1 || legend_items.iter().any(|s| !s.name.is_empty()) {
            for s in legend_items {
                if s.name.is_empty() {
                    continue;
                }
                window.paint_quad(fill(
                    Bounds::new(point(px(lx), px(ly + 5.0)), size(px(10.0), px(3.0))),
                    s.color,
                ));
                let w = paint_text(&s.name, point(px(lx + 14.0), px(ly)), 10.5, fg.opacity(0.85), Align::Left, false, window, cx);
                lx += w + 26.0;
            }
        }
    }

    // ---- hover
    let mouse = window.mouse_position();
    let inside = plot.contains(&mouse);
    track_hover(plot, inside, window);
    if inside {
        let mx = f32::from(mouse.x);
        let my = f32::from(mouse.y);
        let xv = m.inv_x(mx);
        let cross = fg.opacity(0.35);
        vline_quad(mx, pt, pb, cross, window);
        hline_quad(pl, pr, my, cross.opacity(0.2), window);
        let mut rows = vec![];
        let mut x_shown = xv;
        for s in &spec.series {
            let idx = match s.kind {
                SeriesKind::Bars { width } => s
                    .points
                    .iter()
                    .position(|(x, _)| (x - xv).abs() <= width / 2.0),
                _ => nearest_index(&s.points, xv),
            };
            if let Some(i) = idx {
                let (x, y) = s.points[i];
                if rows.is_empty() {
                    x_shown = x;
                }
                if !matches!(s.kind, SeriesKind::Bars { .. }) {
                    let dy = m.ty(y);
                    if dy >= pt && dy <= pb {
                        window.paint_quad(gpui::quad(
                            Bounds::new(point(px(m.tx(x) - 3.0), px(dy - 3.0)), size(px(6.0), px(6.0))),
                            px(3.0),
                            s.color,
                            px(0.0),
                            s.color,
                            gpui::BorderStyle::Solid,
                        ));
                    }
                }
                let name = if s.name.is_empty() { "值".to_string() } else { s.name.to_string() };
                rows.push((s.color, format!("{name}: {}", (spec.y_fmt)(y))));
            }
        }
        if rows.len() > 10 {
            rows.truncate(10);
        }
        let title = format!("x = {}   y = {}", (spec.x_fmt)(x_shown), (spec.y_fmt)(m.inv_y(my)));
        paint_tooltip(mouse, bounds, &title, &rows, window, cx);
    }
}

// ---------------------------------------------------------------------------------------
// Heatmap
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct HeatmapSpec {
    pub row_labels: Vec<String>,
    pub col_labels: Vec<String>,
    /// values[row][col]
    pub values: Vec<Vec<Option<f64>>>,
    pub fmt: Fmt,
    pub row_title: SharedString,
    pub col_title: SharedString,
    /// Higher is better (green) when true.
    pub higher_better: bool,
    pub highlight: Option<(usize, usize)>,
}

pub fn heatmap(spec: Rc<HeatmapSpec>) -> impl IntoElement {
    div().size_full().min_h(px(80.0)).child(
        canvas(|_, _, _| (), move |bounds, _, window, cx| paint_heatmap(&spec, bounds, window, cx))
            .size_full(),
    )
}

/// Diverging red → amber → green colour for t in [0,1].
pub fn diverging(t: f64) -> Hsla {
    let t = t.clamp(0.0, 1.0) as f32;
    // hue 0 (red) .. 0.33 (green)
    let h = 0.0 + 0.33 * t;
    let s = 0.55;
    let l = 0.36 + 0.06 * (1.0 - (2.0 * t - 1.0).abs());
    gpui::hsla(h, s, l, 1.0)
}

fn paint_heatmap(spec: &HeatmapSpec, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let fg = theme.foreground;
    let nr = spec.row_labels.len();
    let nc = spec.col_labels.len();
    if nr == 0 || nc == 0 {
        paint_text("無資料", bounds.center(), 12.0, muted, Align::Center, false, window, cx);
        return;
    }
    let vals: Vec<f64> = spec.values.iter().flatten().flatten().copied().filter(|v| v.is_finite()).collect();
    let lo = vals.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let fs = 10.5;
    let lw = spec
        .row_labels
        .iter()
        .map(|l| text_width(l, fs, true, window, cx))
        .fold(0.0f32, f32::max);
    let left = lw + 26.0;
    let top = 32.0;
    let bottom = 6.0;
    let right = 6.0;
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let gw = f32::from(bounds.size.width) - left - right;
    let gh = f32::from(bounds.size.height) - top - bottom;
    if gw < 20.0 || gh < 20.0 {
        return;
    }
    let cw = gw / nc as f32;
    let ch = gh / nr as f32;
    // titles
    paint_text(&spec.col_title, point(px(ox + left + gw / 2.0), px(oy)), 10.0, muted, Align::Center, false, window, cx);
    for (c, l) in spec.col_labels.iter().enumerate() {
        let x = ox + left + cw * c as f32 + cw / 2.0;
        paint_text(l, point(px(x), px(oy + 15.0)), fs, muted, Align::Center, true, window, cx);
    }
    let show_text = cw >= 34.0 && ch >= 14.0;
    for r in 0..nr {
        let y = oy + top + ch * r as f32;
        paint_text(&spec.row_labels[r], point(px(ox + left - 4.0), px(y + ch / 2.0 - 7.0)), fs, muted, Align::Right, true, window, cx);
        for c in 0..nc {
            let x = ox + left + cw * c as f32;
            let v = spec.values.get(r).and_then(|row| row.get(c)).copied().flatten();
            let color = match v {
                Some(v) if v.is_finite() && hi > lo => {
                    let mut t = (v - lo) / (hi - lo);
                    if !spec.higher_better {
                        t = 1.0 - t;
                    }
                    diverging(t)
                }
                Some(v) if v.is_finite() => diverging(0.5),
                _ => theme.muted,
            };
            let cell = Bounds::new(point(px(x), px(y)), size(px(cw - 1.0), px(ch - 1.0)));
            window.paint_quad(fill(cell, color));
            if spec.highlight == Some((r, c)) {
                window.paint_quad(gpui::outline(cell, fg, gpui::BorderStyle::Solid));
            }
            if show_text {
                if let Some(v) = v {
                    paint_text(&(spec.fmt)(v), point(px(x + cw / 2.0), px(y + ch / 2.0 - 7.0)), 10.0, gpui::white().opacity(0.92), Align::Center, true, window, cx);
                }
            }
        }
    }
    // row title at top-left
    paint_text(&spec.row_title, point(px(ox + 2.0), px(oy)), 10.0, muted, Align::Left, false, window, cx);

    // hover readout
    let grid_bounds = Bounds::new(point(px(ox + left), px(oy + top)), size(px(gw), px(gh)));
    let mouse = window.mouse_position();
    let inside = grid_bounds.contains(&mouse);
    track_hover(grid_bounds, inside, window);
    if inside {
        let c = (((f32::from(mouse.x) - ox - left) / cw) as usize).min(nc - 1);
        let r = (((f32::from(mouse.y) - oy - top) / ch) as usize).min(nr - 1);
        let v = spec.values.get(r).and_then(|row| row.get(c)).copied().flatten();
        let title = format!("{} = {} · {} = {}", spec.row_title, spec.row_labels[r], spec.col_title, spec.col_labels[c]);
        let rows = vec![(fg, v.map(|v| (spec.fmt)(v)).unwrap_or_else(|| "—".into()))];
        paint_tooltip(mouse, bounds, &title, &rows, window, cx);
    }
}
