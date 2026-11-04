use gpui::*;
use gpui_component::*;
struct Hello;
impl Render for Hello {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(gpui::red()).child("選擇權實驗室 OptLab 123.45")
    }
}
fn main() {
    Application::new().run(move |cx| {
        gpui_component::init(cx);
        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), |window, cx| {
                let ms: u64 = std::env::var("SLEEP").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let view = cx.new(|_| Hello);
                cx.new(|cx| Root::new(view, window, cx))
            })?;
            let ms2: u64 = std::env::var("SLEEP2").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
            std::thread::sleep(std::time::Duration::from_millis(ms2));
            Ok::<_, anyhow::Error>(())
        }).detach();
    });
}
