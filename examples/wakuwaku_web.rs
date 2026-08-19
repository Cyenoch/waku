fn main() {
    #[cfg(not(target_family = "wasm"))]
    eprintln!("wakuwaku_web is wasm-only; build it for wasm32-unknown-unknown");
}

#[cfg(target_family = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    use gpui::{App, AppContext, Bounds, WindowBounds, WindowOptions, px, size};

    gpui_platform::web_init();
    gpui_platform::single_threaded_web()
        .with_assets(wakuwaku::assets::Assets)
        .run(|cx: &mut App| {
            wakuwaku::assets::register_fonts(cx).expect("failed to register bundled fonts");
            wakuwaku::input::init(cx);
            wakuwaku::ui::menu::init(cx);
            wakuwaku::theme::init(cx);

            let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| wakuwaku::web::WebApp::new(window, cx)),
            )
            .expect("failed to open WakuWaku web window");
            cx.activate(true);
        });
}
