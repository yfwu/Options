use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;

pub struct BacktestPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl BacktestPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }
}

impl Render for BacktestPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child("backtest (建置中)")
    }
}

#[derive(Clone, Debug, Default)]
pub struct BacktestParamsPatch {}

impl BacktestPage {
    pub fn run(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
    pub fn apply_patch(&mut self, _p: &BacktestParamsPatch, _w: &mut Window, _cx: &mut Context<Self>) {}
}
