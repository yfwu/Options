use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;

pub struct ChainPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl ChainPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }
}

impl Render for ChainPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child("chain (建置中)")
    }
}
impl ChainPage {
    pub fn fetch(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
    pub fn load_demo(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
}
