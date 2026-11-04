use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;

pub struct SettingsPage {
    #[allow(dead_code)]
    state: Entity<AppState>,
}

impl SettingsPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self { state }
    }
}

impl Render for SettingsPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().p_4().child("settings (建置中)")
    }
}
