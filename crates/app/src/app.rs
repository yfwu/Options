//! Root view: sidebar navigation, page host, status bar.

use gpui::{
    actions, div, prelude::*, px, svg, AnyElement, Context, Entity, FocusHandle, Focusable,
    SharedString, Subscription, Window,
};
use gpui_component::{h_flex, v_flex, ActiveTheme, Root};

use crate::pages::{
    backtest::BacktestPage, chain::ChainPage, dashboard::DashboardPage, guide::GuidePage,
    optimizer::OptimizerPage, settings::SettingsPage, strategy::StrategyPage,
};
use crate::state::{AppEvent, AppState, Page, StatusLevel};
use crate::theme::{palette, MONO_FONT, UI_FONT};

actions!(
    optlab,
    [
        GoDashboard,
        GoChain,
        GoStrategy,
        GoBacktest,
        GoOptimizer,
        GoGuide,
        GoSettings,
        ToggleSidebar,
        RunCurrent,
        Quit
    ]
);

pub struct OptLabApp {
    state: Entity<AppState>,
    page: Page,
    collapsed: bool,
    focus: FocusHandle,
    dashboard: Entity<DashboardPage>,
    chain: Entity<ChainPage>,
    strategy: Entity<StrategyPage>,
    backtest: Entity<BacktestPage>,
    optimizer: Entity<OptimizerPage>,
    guide: Entity<GuidePage>,
    settings: Entity<SettingsPage>,
    _subs: Vec<Subscription>,
}

impl OptLabApp {
    pub fn new(page: Page, autorun: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let state = cx.new(|_| AppState::new());
        let dashboard = cx.new(|cx| DashboardPage::new(state.clone(), window, cx));
        let chain = cx.new(|cx| ChainPage::new(state.clone(), window, cx));
        let strategy = cx.new(|cx| StrategyPage::new(state.clone(), window, cx));
        let backtest = cx.new(|cx| BacktestPage::new(state.clone(), window, cx));
        let optimizer = cx.new(|cx| OptimizerPage::new(state.clone(), window, cx));
        let guide = cx.new(|cx| GuidePage::new(state.clone(), window, cx));
        let settings = cx.new(|cx| SettingsPage::new(state.clone(), window, cx));

        let subs = vec![
            cx.subscribe_in(&state, window, |this, _, ev: &AppEvent, window, cx| match ev {
                AppEvent::Navigate(p) => {
                    this.page = *p;
                    cx.notify();
                }
                AppEvent::ApplyBacktestParams(patch) => {
                    let patch = patch.clone();
                    this.backtest.update(cx, |b, cx| b.apply_patch(&patch, window, cx));
                    this.page = Page::Backtest;
                    cx.notify();
                }
                AppEvent::ChainChanged => {
                    this.strategy.update(cx, |s, cx| s.on_chain_changed(window, cx));
                }
                AppEvent::SettingsChanged => {}
            }),
            cx.observe(&state, |_, _, cx| cx.notify()),
        ];

        let focus = cx.focus_handle();
        window.focus(&focus);

        let this = Self {
            state,
            page,
            collapsed: false,
            focus,
            dashboard,
            chain,
            strategy,
            backtest,
            optimizer,
            guide,
            settings,
            _subs: subs,
        };

        if autorun {
            match page {
                Page::Backtest => this.backtest.update(cx, |p, cx| p.run(window, cx)),
                Page::Optimizer => this.optimizer.update(cx, |p, cx| p.run(window, cx)),
                Page::Chain => this.chain.update(cx, |p, cx| p.load_demo(window, cx)),
                Page::Strategy => {
                    this.chain.update(cx, |p, cx| p.load_demo(window, cx));
                }
                _ => {}
            }
        }
        this
    }

    fn go(&mut self, p: Page, cx: &mut Context<Self>) {
        self.page = p;
        cx.notify();
    }

    fn run_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.page {
            Page::Backtest => self.backtest.update(cx, |p, cx| p.run(window, cx)),
            Page::Optimizer => self.optimizer.update(cx, |p, cx| p.run(window, cx)),
            Page::Chain => self.chain.update(cx, |p, cx| p.fetch(window, cx)),
            Page::Strategy => self.strategy.update(cx, |p, cx| p.rebuild(window, cx)),
            Page::Dashboard => self.dashboard.update(cx, |p, cx| p.reload(window, cx)),
            _ => {}
        }
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let p = palette();
        let collapsed = self.collapsed;
        let width = if collapsed { px(44.0) } else { px(188.0) };
        #[cfg(target_os = "macos")]
        let mod_key = "⌘";
        #[cfg(not(target_os = "macos"))]
        let mod_key = "Ctrl+";
        v_flex()
            .w(width)
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id("brand")
                    .h(px(40.0))
                    .px_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.collapsed = !this.collapsed;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(20.0))
                            .flex_none()
                            .rounded(px(4.0))
                            .bg(p.blue)
                            .text_color(p.bg)
                            .text_size(px(12.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("Δ"),
                    )
                    .when(!collapsed, |d| {
                        d.child(
                            v_flex()
                                .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::SEMIBOLD).child("OptLab"))
                                .child(div().text_size(px(10.5)).text_color(theme.muted_foreground).child("選擇權實驗室")),
                        )
                    }),
            )
            .child(
                v_flex().p_1().gap_0p5().flex_1().children(Page::ALL.iter().enumerate().map(|(i, pg)| {
                    let active = *pg == self.page;
                    let pg = *pg;
                    let shortcut = if pg == Page::Settings { format!("{mod_key},") } else { format!("{mod_key}{}", i + 1) };
                    h_flex()
                        .id(SharedString::from(format!("nav-{}", pg.key())))
                        .h(px(30.0))
                        .px_2()
                        .gap_2()
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .text_size(px(12.5))
                        .when(active, |d| d.bg(p.blue.opacity(0.16)).text_color(p.text))
                        .when(!active, |d| d.text_color(p.muted).hover(|s| s.bg(p.elevated)))
                        .on_click(cx.listener(move |this, _, _, cx| this.go(pg, cx)))
                        .child(
                            svg()
                                .path(pg.icon())
                                .size(px(15.0))
                                .flex_none()
                                .text_color(if active { p.blue } else { p.muted }),
                        )
                        .when(!collapsed, |d| {
                            d.child(div().flex_1().child(pg.title())).child(
                                div().text_size(px(10.0)).text_color(p.dim).font_family(MONO_FONT).child(shortcut),
                            )
                        })
                })),
            )
            .when(!collapsed, |d| {
                d.child(
                    v_flex()
                        .p_3()
                        .gap_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_size(px(10.5))
                        .text_color(p.dim)
                        .child("長期看多 · Rolling LEAPS")
                        .child("QQQ / SPY / SPX / NDX")
                        .child(format!("{mod_key}B 收合側欄 · {mod_key}Enter 執行")),
                )
            })
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let p = palette();
        let st = self.state.read(cx);
        let (level, msg) = st.status.clone();
        let color = match level {
            StatusLevel::Info => p.muted,
            StatusLevel::Ok => p.green,
            StatusLevel::Warn => p.yellow,
            StatusLevel::Error => p.red,
        };
        let dot = match level {
            StatusLevel::Info => "●",
            StatusLevel::Ok => "✓",
            StatusLevel::Warn => "▲",
            StatusLevel::Error => "✕",
        };
        let busy: Vec<SharedString> = st.busy.iter().map(|(_, l)| l.clone()).collect();
        let chain = st
            .chain
            .as_ref()
            .map(|c| format!("鏈: {} {} @ {:.2} · {} 檔 · {}", c.underlying, c.as_of.format("%Y-%m-%d %H:%M"), c.spot, c.quotes.len(), c.source))
            .unwrap_or_else(|| "鏈: 未載入".into());
        let settings = &st.settings;
        let rq = format!("r {:.2}% · q {:.2}%", settings.risk_free * 100.0, settings.dividend_yield * 100.0);
        h_flex()
            .h(px(24.0))
            .flex_none()
            .px_2()
            .gap_3()
            .bg(p.status_bar)
            .border_t_1()
            .border_color(theme.border)
            .text_size(px(11.0))
            .text_color(p.muted)
            .child(
                h_flex()
                    .gap_1()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .child(div().text_color(color).child(dot))
                    .child(div().text_color(color).truncate().child(msg)),
            )
            .when(!busy.is_empty(), |d| {
                d.child(
                    h_flex()
                        .gap_1()
                        .text_color(p.blue)
                        .child("⟳")
                        .child(busy.join(" · ")),
                )
            })
            .child(div().font_family(MONO_FONT).child(chain))
            .child(div().font_family(MONO_FONT).child(rq))
            .child(div().text_color(p.dim).child(format!("{} · {}", self.page.title(), self.page.subtitle())))
    }

    fn render_page(&self) -> AnyElement {
        match self.page {
            Page::Dashboard => self.dashboard.clone().into_any_element(),
            Page::Chain => self.chain.clone().into_any_element(),
            Page::Strategy => self.strategy.clone().into_any_element(),
            Page::Backtest => self.backtest.clone().into_any_element(),
            Page::Optimizer => self.optimizer.clone().into_any_element(),
            Page::Guide => self.guide.clone().into_any_element(),
            Page::Settings => self.settings.clone().into_any_element(),
        }
    }
}

impl Focusable for OptLabApp {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for OptLabApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);
        let sidebar = self.render_sidebar(cx).into_any_element();
        let status_bar = self.render_status_bar(cx).into_any_element();
        let theme = cx.theme();
        let p = palette();
        v_flex()
            .id("optlab-root")
            .key_context("OptLab")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .font_family(UI_FONT)
            .text_size(px(13.0))
            .on_action(cx.listener(|this, _: &GoDashboard, _, cx| this.go(Page::Dashboard, cx)))
            .on_action(cx.listener(|this, _: &GoChain, _, cx| this.go(Page::Chain, cx)))
            .on_action(cx.listener(|this, _: &GoStrategy, _, cx| this.go(Page::Strategy, cx)))
            .on_action(cx.listener(|this, _: &GoBacktest, _, cx| this.go(Page::Backtest, cx)))
            .on_action(cx.listener(|this, _: &GoOptimizer, _, cx| this.go(Page::Optimizer, cx)))
            .on_action(cx.listener(|this, _: &GoGuide, _, cx| this.go(Page::Guide, cx)))
            .on_action(cx.listener(|this, _: &GoSettings, _, cx| this.go(Page::Settings, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.collapsed = !this.collapsed;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RunCurrent, window, cx| this.run_current(window, cx)))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                h_flex()
                                    .h(px(40.0))
                                    .flex_none()
                                    .px_3()
                                    .gap_2()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .bg(theme.title_bar)
                                    .child(div().text_size(px(14.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(self.page.title()))
                                    .child(div().text_size(px(11.5)).text_color(p.dim).child(self.page.subtitle())),
                            )
                            .child(div().flex_1().min_h_0().child(self.render_page())),
                    ),
            )
            .child(status_bar)
            .children(dialog_layer)
            .children(notification_layer)
            .into_any_element()
    }
}
