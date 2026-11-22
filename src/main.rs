#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod actions;
mod audience;
mod cache_policy;
mod docks;
mod pdf;
mod presentation;
mod presenter;
mod settings;
mod slide_view;
mod timer;
mod view_raster;

use gpui_kit::{
    AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use crate::{
    actions::{
        EndPresentation, NextSlide, OpenPdf, PreviousSlide, StartPresentation, ToggleFullscreen,
    },
    pdf::PdfWorker,
    presentation::Presentation,
    presenter::Presenter,
    settings::Settings,
};
use gpui_kit::component::theme::Theme;

const APP_ID: &str = "app.elafra.Elafra";

fn main() {
    #[cfg(target_os = "linux")]
    let app = {
        if std::env::var_os("DISPLAY").is_none_or(|display| display.is_empty()) {
            eprintln!("Elafra の起動には X11 または XWayland が必要。");
            std::process::exit(1);
        }
        gpui_kit::platform::linux(gpui_kit::WindowingModes::X11)
    };
    #[cfg(not(target_os = "linux"))]
    let app = gpui_kit::application();

    app.with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::set_locale("ja");
        let open_key = if cfg!(target_os = "macos") {
            "cmd-o"
        } else {
            "ctrl-o"
        };
        cx.bind_keys([
            KeyBinding::new(open_key, OpenPdf, Some("Presenter")),
            KeyBinding::new("f5", StartPresentation, Some("Presenter")),
            KeyBinding::new("escape", EndPresentation, Some("Presenter || Audience")),
            KeyBinding::new("f11", ToggleFullscreen, Some("Presenter || Audience")),
            KeyBinding::new("right", NextSlide, Some("Presenter || Audience")),
            KeyBinding::new("pagedown", NextSlide, Some("Presenter || Audience")),
            KeyBinding::new("left", PreviousSlide, Some("Presenter || Audience")),
            KeyBinding::new("pageup", PreviousSlide, Some("Presenter || Audience")),
        ]);

        let (settings, notice) = match Settings::load() {
            Ok(settings) => (settings, None),
            Err(error) => (
                Settings::default(),
                Some(format!(
                    "設定を読み込めなかったため、初期設定で開いた。{error}"
                )),
            ),
        };
        Theme::change(settings.theme.gpui_mode(), None, cx);
        let (worker, pdf_events) = PdfWorker::start();
        let model = cx.new(|_| Presentation::new(settings, worker, notice));
        let bounds = Bounds::centered(None, size(px(1240.), px(820.)), cx);
        let presenter = gpui_kit::open_window(
            WindowOptions {
                app_id: Some(APP_ID.into()),
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(560.), px(380.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Elafra — プレゼンター".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let size = audience::largest_raster_size(cx);
                model.update(cx, |model, cx| model.set_raster_size(size, cx));
                cx.new(|cx| Presenter::new(model.clone(), window, cx))
            },
        );
        let presenter = match presenter {
            Ok((handle, _)) => handle,
            Err(error) => {
                eprintln!("プレゼンター画面を開けなかった。{error}");
                cx.quit();
                return;
            }
        };

        let display_model = model.downgrade();
        cx.observe_displays(move |_, cx| {
            let display_model = display_model.clone();
            cx.defer(move |cx| {
                let size = audience::largest_raster_size(cx);
                let _ = display_model.update(cx, |model, cx| model.set_raster_size(size, cx));
            });
        })
        .detach();

        // GPUI's X11 backend does not publish RandR mode changes.
        #[cfg(target_os = "linux")]
        {
            let display_model = model.downgrade();
            cx.spawn(async move |cx| {
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(2))
                        .await;
                    let open = cx.update(|cx| {
                        let Some(model) = display_model.upgrade() else {
                            return false;
                        };
                        if model.read(cx).page_count() == 0 {
                            return true;
                        }
                        let size = audience::largest_raster_size(cx);
                        model.update(cx, |model, cx| model.set_raster_size(size, cx));
                        true
                    });
                    if !open {
                        break;
                    }
                }
            })
            .detach();
        }

        let presenter_id = presenter.window_id();
        let close_model = model.clone();
        cx.on_window_closed(move |cx, id| {
            if id == presenter_id {
                close_model.update(cx, |model, cx| {
                    model.flush_settings(cx);
                    model.end_presentation(cx);
                });
                cx.quit();
            } else {
                close_model.update(cx, |model, cx| model.audience_closed(id, cx));
            }
        })
        .detach();

        let quit_model = model.clone();
        cx.on_app_quit(move |cx| {
            quit_model.update(cx, |model, cx| model.flush_settings(cx));
            async {}
        })
        .detach();

        let pdf_model = model.downgrade();
        cx.spawn(async move |cx| {
            while let Ok(event) = pdf_events.recv().await {
                if pdf_model
                    .update(cx, |model, cx| model.handle_pdf_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        cx.activate(true);
    });
}
