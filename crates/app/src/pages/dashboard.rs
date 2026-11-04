use gpui::{div, prelude::*, Context, Entity, Window};
use crate::state::AppState;
pub struct DashboardPage { #[allow(dead_code)] state: Entity<AppState> }
impl DashboardPage {
    pub fn new(state: Entity<AppState>, _window: &mut Window, _cx: &mut Context<Self>) -> Self { Self { state } }
    pub fn reload(&mut self, _w: &mut Window, _cx: &mut Context<Self>) {}
}
impl Render for DashboardPage {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement { div().child("dash") }
}
