use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;

pub struct StrategyPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl StrategyPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }
}

impl Render for StrategyPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child("strategy (建置中)")
    }
}
impl StrategyPage {
    pub fn rebuild(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
    pub fn on_chain_changed(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
}
