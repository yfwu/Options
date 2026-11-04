//! OptLab 選擇權實驗室 — desktop GUI (GPUI + gpui-component).
//!
//! CLI / env options:
//! * `--page <dashboard|chain|strategy|backtest|optimizer|guide|settings>` or `OPTLAB_PAGE`
//! * `OPTLAB_AUTORUN=1` — auto-run the default backtest / optimiser / chain demo on launch
//! * `OPTLAB_WINDOW=1600x1000` — initial window size

mod app;
mod assets;
mod data_bridge;
mod pages;
mod state;
mod theme;
mod widgets;

use gpui::{
    px, size, App, AppContext, Application, Bounds, KeyBinding, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use gpui_component::Root;

use crate::app::*;
use crate::state::Page;

pub struct LaunchOptions {
    pub page: Page,
    pub autorun: bool,
}

fn launch_options() -> LaunchOptions {
    let mut page = std::env::var("OPTLAB_PAGE").ok().and_then(|p| Page::from_key(&p));
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        if a == "--page" {
            if let Some(v) = args.get(i + 1) {
                page = Page::from_key(v).or(page);
                i += 1;
            }
        } else if let Some(v) = a.strip_prefix("--page=") {
            page = Page::from_key(v).or(page);
        } else if a == "--help" || a == "-h" {
            println!(
                "optlab-app [--page dashboard|chain|strategy|backtest|optimizer|guide|settings]\n\
                 env: OPTLAB_PAGE, OPTLAB_AUTORUN=1, OPTLAB_WINDOW=WxH"
            );
            std::process::exit(0);
        }
        i += 1;
    }
    let autorun = std::env::var("OPTLAB_AUTORUN").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false);
    LaunchOptions { page: page.unwrap_or(Page::Dashboard), autorun }
}

fn window_size() -> (f32, f32) {
    std::env::var("OPTLAB_WINDOW")
        .ok()
        .and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or((1480.0, 920.0))
}

fn bind_keys(cx: &mut App) {
    #[cfg(target_os = "macos")]
    let m = "cmd";
    #[cfg(not(target_os = "macos"))]
    let m = "ctrl";
    cx.bind_keys([
        KeyBinding::new(&format!("{m}-1"), GoDashboard, None),
        KeyBinding::new(&format!("{m}-2"), GoChain, None),
        KeyBinding::new(&format!("{m}-3"), GoStrategy, None),
        KeyBinding::new(&format!("{m}-4"), GoBacktest, None),
        KeyBinding::new(&format!("{m}-5"), GoOptimizer, None),
        KeyBinding::new(&format!("{m}-6"), GoGuide, None),
        KeyBinding::new(&format!("{m}-,"), GoSettings, None),
        KeyBinding::new(&format!("{m}-b"), ToggleSidebar, None),
        KeyBinding::new(&format!("{m}-enter"), RunCurrent, None),
        KeyBinding::new(&format!("{m}-q"), Quit, None),
    ]);
}

fn main() {
    let opts = launch_options();
    let app = Application::new().with_assets(assets::Assets);

    app.run(move |cx| {
        gpui_component::init(cx);
        theme::apply(cx);
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let (w, h) = window_size();
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        let page = opts.page;
        let autorun = opts.autorun;
        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("OptLab 選擇權實驗室".into()),
                        ..Default::default()
                    }),
                    window_min_size: Some(size(px(960.0), px(600.0))),
                    app_id: Some("optlab".into()),
                    ..Default::default()
                },
                |window, cx| {
                    // Re-apply our palette in case gpui-component synced to the system appearance.
                    theme::apply(cx);
                    let view = cx.new(|cx| OptLabApp::new(page, autorun, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
