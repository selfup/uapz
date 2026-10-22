//! uapz: a node graph editor built on gpui-kit.

mod doc;
mod graph;
mod sidebar;

use gpui_kit::*;
use graph::{AddNode, Cancel, DeleteSelection};
use sidebar::{NewGraph, Save, Workspace};

gpui_kit::actions!(app, [Quit]);

fn main() {
    gpui_kit::application().run(|cx| {
        gpui_kit::init(cx);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-s", Save, Some(sidebar::KEY_CONTEXT)),
            KeyBinding::new("ctrl-s", Save, Some(sidebar::KEY_CONTEXT)),
            KeyBinding::new("cmd-shift-n", NewGraph, Some(sidebar::KEY_CONTEXT)),
            KeyBinding::new("cmd-n", AddNode, Some(graph::KEY_CONTEXT)),
            KeyBinding::new("ctrl-n", AddNode, Some(graph::KEY_CONTEXT)),
            KeyBinding::new("backspace", DeleteSelection, Some(graph::KEY_CONTEXT)),
            KeyBinding::new("delete", DeleteSelection, Some(graph::KEY_CONTEXT)),
            KeyBinding::new("escape", Cancel, Some(graph::KEY_CONTEXT)),
        ]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1240.0), px(760.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("uapz".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Workspace::new(window, cx))
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
