//! Embedded SVG icon assets (Lucide, ISC licence; gpui-component icon set, Apache-2.0).
// @generated list of files in crates/app/assets/icons

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

static ICONS: &[(&str, &[u8])] = &[
    ("icons/function.svg", include_bytes!("../assets/icons/function.svg")),
    ("icons/a-large-small.svg", include_bytes!("../assets/icons/a-large-small.svg")),
    ("icons/arrow-down.svg", include_bytes!("../assets/icons/arrow-down.svg")),
    ("icons/arrow-left.svg", include_bytes!("../assets/icons/arrow-left.svg")),
    ("icons/arrow-right.svg", include_bytes!("../assets/icons/arrow-right.svg")),
    ("icons/arrow-up.svg", include_bytes!("../assets/icons/arrow-up.svg")),
    ("icons/asterisk.svg", include_bytes!("../assets/icons/asterisk.svg")),
    ("icons/bell-ring.svg", include_bytes!("../assets/icons/bell-ring.svg")),
    ("icons/bell.svg", include_bytes!("../assets/icons/bell.svg")),
    ("icons/book-open.svg", include_bytes!("../assets/icons/book-open.svg")),
    ("icons/bot.svg", include_bytes!("../assets/icons/bot.svg")),
    ("icons/building-2.svg", include_bytes!("../assets/icons/building-2.svg")),
    ("icons/calendar.svg", include_bytes!("../assets/icons/calendar.svg")),
    ("icons/case-sensitive.svg", include_bytes!("../assets/icons/case-sensitive.svg")),
    ("icons/chart-line.svg", include_bytes!("../assets/icons/chart-line.svg")),
    ("icons/chart-pie.svg", include_bytes!("../assets/icons/chart-pie.svg")),
    ("icons/check.svg", include_bytes!("../assets/icons/check.svg")),
    ("icons/chevron-down.svg", include_bytes!("../assets/icons/chevron-down.svg")),
    ("icons/chevron-left.svg", include_bytes!("../assets/icons/chevron-left.svg")),
    ("icons/chevron-right.svg", include_bytes!("../assets/icons/chevron-right.svg")),
    ("icons/chevron-up.svg", include_bytes!("../assets/icons/chevron-up.svg")),
    ("icons/chevrons-up-down.svg", include_bytes!("../assets/icons/chevrons-up-down.svg")),
    ("icons/circle-check.svg", include_bytes!("../assets/icons/circle-check.svg")),
    ("icons/circle-user.svg", include_bytes!("../assets/icons/circle-user.svg")),
    ("icons/circle-x.svg", include_bytes!("../assets/icons/circle-x.svg")),
    ("icons/close.svg", include_bytes!("../assets/icons/close.svg")),
    ("icons/copy.svg", include_bytes!("../assets/icons/copy.svg")),
    ("icons/dash.svg", include_bytes!("../assets/icons/dash.svg")),
    ("icons/delete.svg", include_bytes!("../assets/icons/delete.svg")),
    ("icons/download.svg", include_bytes!("../assets/icons/download.svg")),
    ("icons/ellipsis-vertical.svg", include_bytes!("../assets/icons/ellipsis-vertical.svg")),
    ("icons/ellipsis.svg", include_bytes!("../assets/icons/ellipsis.svg")),
    ("icons/external-link.svg", include_bytes!("../assets/icons/external-link.svg")),
    ("icons/eye-off.svg", include_bytes!("../assets/icons/eye-off.svg")),
    ("icons/eye.svg", include_bytes!("../assets/icons/eye.svg")),
    ("icons/file.svg", include_bytes!("../assets/icons/file.svg")),
    ("icons/flask-conical.svg", include_bytes!("../assets/icons/flask-conical.svg")),
    ("icons/folder-closed.svg", include_bytes!("../assets/icons/folder-closed.svg")),
    ("icons/folder-open.svg", include_bytes!("../assets/icons/folder-open.svg")),
    ("icons/folder.svg", include_bytes!("../assets/icons/folder.svg")),
    ("icons/frame.svg", include_bytes!("../assets/icons/frame.svg")),
    ("icons/gallery-vertical-end.svg", include_bytes!("../assets/icons/gallery-vertical-end.svg")),
    ("icons/github.svg", include_bytes!("../assets/icons/github.svg")),
    ("icons/globe.svg", include_bytes!("../assets/icons/globe.svg")),
    ("icons/heart-off.svg", include_bytes!("../assets/icons/heart-off.svg")),
    ("icons/heart.svg", include_bytes!("../assets/icons/heart.svg")),
    ("icons/history.svg", include_bytes!("../assets/icons/history.svg")),
    ("icons/inbox.svg", include_bytes!("../assets/icons/inbox.svg")),
    ("icons/info.svg", include_bytes!("../assets/icons/info.svg")),
    ("icons/inspector.svg", include_bytes!("../assets/icons/inspector.svg")),
    ("icons/layout-dashboard.svg", include_bytes!("../assets/icons/layout-dashboard.svg")),
    ("icons/loader-circle.svg", include_bytes!("../assets/icons/loader-circle.svg")),
    ("icons/loader.svg", include_bytes!("../assets/icons/loader.svg")),
    ("icons/map.svg", include_bytes!("../assets/icons/map.svg")),
    ("icons/maximize.svg", include_bytes!("../assets/icons/maximize.svg")),
    ("icons/menu.svg", include_bytes!("../assets/icons/menu.svg")),
    ("icons/minimize.svg", include_bytes!("../assets/icons/minimize.svg")),
    ("icons/minus.svg", include_bytes!("../assets/icons/minus.svg")),
    ("icons/monitor.svg", include_bytes!("../assets/icons/monitor.svg")),
    ("icons/moon.svg", include_bytes!("../assets/icons/moon.svg")),
    ("icons/palette.svg", include_bytes!("../assets/icons/palette.svg")),
    ("icons/panel-bottom-open.svg", include_bytes!("../assets/icons/panel-bottom-open.svg")),
    ("icons/panel-bottom.svg", include_bytes!("../assets/icons/panel-bottom.svg")),
    ("icons/panel-left-close.svg", include_bytes!("../assets/icons/panel-left-close.svg")),
    ("icons/panel-left-open.svg", include_bytes!("../assets/icons/panel-left-open.svg")),
    ("icons/panel-left.svg", include_bytes!("../assets/icons/panel-left.svg")),
    ("icons/panel-right-close.svg", include_bytes!("../assets/icons/panel-right-close.svg")),
    ("icons/panel-right-open.svg", include_bytes!("../assets/icons/panel-right-open.svg")),
    ("icons/panel-right.svg", include_bytes!("../assets/icons/panel-right.svg")),
    ("icons/play.svg", include_bytes!("../assets/icons/play.svg")),
    ("icons/plus.svg", include_bytes!("../assets/icons/plus.svg")),
    ("icons/redo-2.svg", include_bytes!("../assets/icons/redo-2.svg")),
    ("icons/redo.svg", include_bytes!("../assets/icons/redo.svg")),
    ("icons/refresh-cw.svg", include_bytes!("../assets/icons/refresh-cw.svg")),
    ("icons/replace.svg", include_bytes!("../assets/icons/replace.svg")),
    ("icons/resize-corner.svg", include_bytes!("../assets/icons/resize-corner.svg")),
    ("icons/save.svg", include_bytes!("../assets/icons/save.svg")),
    ("icons/search.svg", include_bytes!("../assets/icons/search.svg")),
    ("icons/settings-2.svg", include_bytes!("../assets/icons/settings-2.svg")),
    ("icons/settings.svg", include_bytes!("../assets/icons/settings.svg")),
    ("icons/sliders-horizontal.svg", include_bytes!("../assets/icons/sliders-horizontal.svg")),
    ("icons/sort-ascending.svg", include_bytes!("../assets/icons/sort-ascending.svg")),
    ("icons/sort-descending.svg", include_bytes!("../assets/icons/sort-descending.svg")),
    ("icons/square-terminal.svg", include_bytes!("../assets/icons/square-terminal.svg")),
    ("icons/star-off.svg", include_bytes!("../assets/icons/star-off.svg")),
    ("icons/star.svg", include_bytes!("../assets/icons/star.svg")),
    ("icons/sun.svg", include_bytes!("../assets/icons/sun.svg")),
    ("icons/table.svg", include_bytes!("../assets/icons/table.svg")),
    ("icons/thumbs-down.svg", include_bytes!("../assets/icons/thumbs-down.svg")),
    ("icons/thumbs-up.svg", include_bytes!("../assets/icons/thumbs-up.svg")),
    ("icons/triangle-alert.svg", include_bytes!("../assets/icons/triangle-alert.svg")),
    ("icons/undo-2.svg", include_bytes!("../assets/icons/undo-2.svg")),
    ("icons/undo.svg", include_bytes!("../assets/icons/undo.svg")),
    ("icons/user.svg", include_bytes!("../assets/icons/user.svg")),
    ("icons/window-close.svg", include_bytes!("../assets/icons/window-close.svg")),
    ("icons/window-maximize.svg", include_bytes!("../assets/icons/window-maximize.svg")),
    ("icons/window-minimize.svg", include_bytes!("../assets/icons/window-minimize.svg")),
    ("icons/window-restore.svg", include_bytes!("../assets/icons/window-restore.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(p, _)| *p == path).map(|(_, d)| Cow::Borrowed(*d)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| SharedString::from(*p))
            .collect())
    }
}
