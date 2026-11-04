use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;

pub struct GuidePage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl GuidePage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }
}

impl Render for GuidePage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child("guide (建置中)")
    }
}
