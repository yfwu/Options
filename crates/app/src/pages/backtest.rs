use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;
#[derive(Clone, Debug, Default)]
pub struct BacktestParamsPatch { pub symbol: Option<String>, pub config: Option<optlab_core::backtest::RollConfig>, pub run: bool }
pub struct BacktestPage { #[allow(dead_code)] state: Entity<AppState> }
impl BacktestPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self { Self { state } }
    pub fn run(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
    pub fn apply_patch(&mut self, _p: &BacktestParamsPatch, _w: &mut Window, _cx: &mut Context<Self>) {}
}
impl Render for BacktestPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement { div().child("bt") }
}
