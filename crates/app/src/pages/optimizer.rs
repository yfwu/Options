use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;

pub struct OptimizerPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl OptimizerPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }
}

impl Render for OptimizerPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child("optimizer (建置中)")
    }
}
impl OptimizerPage {
    pub fn run(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
}
