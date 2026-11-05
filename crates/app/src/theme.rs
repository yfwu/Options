//! Zed "One Dark"-inspired theme applied on top of gpui-component's Theme.

use gpui::{px, rgb, App, Hsla, SharedString};
use gpui_component::{Theme, ThemeMode};

/// Semantic colours used by charts / tables (independent of gpui-component).
#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Hsla,
    pub panel: Hsla,
    pub surface: Hsla,
    pub elevated: Hsla,
    pub border: Hsla,
    pub border_variant: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub dim: Hsla,
    pub blue: Hsla,
    pub green: Hsla,
    pub red: Hsla,
    pub yellow: Hsla,
    pub orange: Hsla,
    pub purple: Hsla,
    pub cyan: Hsla,
    pub status_bar: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn palette() -> Palette {
    Palette {
        bg: c(0x282c33),
        panel: c(0x2f343e),
        surface: c(0x2e343e),
        elevated: c(0x363c46),
        border: c(0x464b57),
        border_variant: c(0x363c46),
        text: c(0xdce0e5),
        muted: c(0xa9afbc),
        dim: c(0x878a98),
        blue: c(0x74ade8),
        green: c(0xa1c181),
        red: c(0xd07277),
        yellow: c(0xdec184),
        orange: c(0xd19a66),
        purple: c(0xb477cf),
        cyan: c(0x6eb4bf),
        status_bar: c(0x2f343e),
    }
}

pub const UI_FONT: &str = if cfg!(target_os = "macos") {
    "PingFang TC"
} else if cfg!(target_os = "windows") {
    "Microsoft JhengHei UI"
} else {
    "Noto Sans CJK TC"
};

pub const MONO_FONT: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(target_os = "windows") {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let p = palette();
    let t = Theme::global_mut(cx);
    t.font_family = SharedString::from(UI_FONT);
    t.mono_font_family = SharedString::from(MONO_FONT);
    t.font_size = px(13.0);
    t.mono_font_size = px(12.0);
    t.radius = px(4.0);
    t.radius_lg = px(6.0);
    t.shadow = false;

    t.background = p.bg;
    t.foreground = p.text;
    t.border = p.border;
    t.input = p.border;
    t.ring = p.blue;
    t.caret = p.blue;
    t.selection = p.blue.opacity(0.3);
    t.muted = p.surface;
    t.muted_foreground = p.muted;
    t.accent = p.elevated;
    t.accent_foreground = p.text;
    t.primary = c(0x3a5d86);
    t.primary_hover = c(0x44699a);
    t.primary_active = c(0x30507a);
    t.primary_foreground = p.text;
    t.secondary = p.surface;
    t.secondary_hover = p.elevated;
    t.secondary_active = c(0x3d434f);
    t.secondary_foreground = p.text;
    t.popover = p.panel;
    t.popover_foreground = p.text;
    t.list = p.bg;
    t.list_hover = p.elevated;
    t.list_active = p.blue.opacity(0.18);
    t.list_active_border = p.blue;
    t.list_even = p.surface.opacity(0.4);
    t.list_head = p.panel;
    t.table = p.bg;
    t.table_head = p.panel;
    t.table_head_foreground = p.muted;
    t.table_even = p.surface.opacity(0.5);
    t.table_hover = p.elevated;
    t.table_active = p.blue.opacity(0.18);
    t.table_active_border = p.blue;
    t.table_row_border = p.border_variant;
    t.sidebar = p.panel;
    t.sidebar_border = p.border;
    t.sidebar_foreground = p.text;
    t.sidebar_accent = p.elevated;
    t.sidebar_accent_foreground = p.text;
    t.sidebar_primary = p.blue;
    t.sidebar_primary_foreground = p.bg;
    t.title_bar = p.panel;
    t.title_bar_border = p.border;
    t.tab_bar = p.panel;
    t.tab = p.panel;
    t.tab_active = p.bg;
    t.tab_active_foreground = p.text;
    t.tab_foreground = p.muted;
    t.progress_bar = p.blue;
    t.scrollbar = gpui::transparent_black();
    t.scrollbar_thumb = p.border.opacity(0.7);
    t.scrollbar_thumb_hover = p.border;
    t.success = p.green;
    t.danger = p.red;
    t.warning = p.yellow;
    t.info = p.blue;
    t.bullish = p.green;
    t.bearish = p.red;
    t.chart_1 = p.blue;
    t.chart_2 = p.green;
    t.chart_3 = p.orange;
    t.chart_4 = p.purple;
    t.chart_5 = p.cyan;
    t.window_border = p.border;
    t.overlay = gpui::black().opacity(0.4);
    // code editor (Payoff Studio) colours
    let mut ht = (*gpui_component::highlighter::HighlightTheme::default_dark()).clone();
    ht.style.editor_background = Some(p.bg);
    ht.style.editor_foreground = Some(p.text);
    ht.style.editor_active_line = Some(p.surface);
    ht.style.editor_line_number = Some(p.dim);
    ht.style.editor_active_line_number = Some(p.muted);
    t.highlight_theme = std::sync::Arc::new(ht);
}
