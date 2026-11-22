use std::{sync::Arc, time::Duration};

use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Context, DisplayId, Entity, FocusHandle, ImageId,
    InteractiveElement, IntoElement, ParentElement, Pixels, Point, Render, RenderImage, Result,
    Styled, Subscription, Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowId,
    WindowOptions, canvas, div, point, px, rgb, size,
};

use crate::{
    actions::{EndPresentation, NextSlide, PreviousSlide, ToggleFullscreen},
    pdf::RasterSize,
    presentation::Presentation,
    settings::SlideFit,
    slide_view::{fitted_page, page_placement, paint_page, raster_size},
    view_raster::{
        DisplayPage, MAX_TEXTURE_EDGE, PageStamp, SlideViewport, ViewRequest, ViewTarget,
    },
};

const PAGE_SETTLE_DELAY: Duration = Duration::from_millis(80);
const PLACEMENT_INTERVAL: Duration = Duration::from_millis(75);
const PLACEMENT_CHECKS: usize = 20;
const FALLBACK_RASTER_SIZE: RasterSize = RasterSize {
    width: 1920,
    height: 1080,
};

pub(crate) struct Monitor {
    pub id: DisplayId,
    pub label: String,
    pub is_presenter: bool,
}

#[derive(Clone, Copy)]
struct Output {
    id: DisplayId,
    bounds: Bounds<Pixels>,
}

struct Desktop {
    outputs: Vec<(Output, Option<String>)>,
    presenter: Option<DisplayId>,
}

pub(crate) struct Target {
    output: Option<Output>,
    window_bounds: Bounds<Pixels>,
    fullscreen: bool,
}

impl Target {
    pub(crate) fn id(&self) -> Option<DisplayId> {
        self.output.map(|output| output.id)
    }
}

pub(crate) fn displays(presenter: &Window, cx: &App) -> Vec<Monitor> {
    let desktop = desktop(presenter, cx);
    desktop
        .outputs
        .iter()
        .enumerate()
        .map(|(index, (output, name))| {
            let resolution = format!(
                "{}×{}",
                output.bounds.size.width.to_i32(),
                output.bounds.size.height.to_i32()
            );
            let label = match name {
                Some(name) => format!("画面 {}  {name} ({resolution})", index + 1),
                None => format!("画面 {}  ({resolution})", index + 1),
            };
            Monitor {
                id: output.id,
                label,
                is_presenter: desktop.presenter == Some(output.id),
            }
        })
        .collect()
}

pub(crate) fn largest_raster_size(cx: &App) -> RasterSize {
    select_largest_raster_size(active_raster_sizes(cx))
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn active_raster_sizes(cx: &App) -> Vec<RasterSize> {
    cx.displays()
        .into_iter()
        .filter_map(|display| display_raster_size(display.id(), display.bounds()))
        .collect()
}

#[cfg(target_os = "linux")]
fn active_raster_sizes(_: &App) -> Vec<RasterSize> {
    x11::raster_sizes().unwrap_or_default()
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn active_raster_sizes(_: &App) -> Vec<RasterSize> {
    Vec::new()
}

fn select_largest_raster_size(resolutions: impl IntoIterator<Item = RasterSize>) -> RasterSize {
    resolutions
        .into_iter()
        .filter(|size| size.width > 0 && size.height > 0)
        .max_by_key(|size| {
            (
                u64::from(size.width) * u64::from(size.height),
                size.width.max(size.height),
                size.width,
            )
        })
        .unwrap_or(FALLBACK_RASTER_SIZE)
}

pub(crate) fn resolve(requested: Option<DisplayId>, presenter: &Window, cx: &App) -> Target {
    let desktop = desktop(presenter, cx);
    let output = select_output(&desktop, requested);
    let area = output.map_or_else(|| presenter.bounds(), |output| output.bounds);
    let extent = size(
        (area.size.width * 0.7).min(px(960.)),
        (area.size.height * 0.7).min(px(540.)),
    );
    let center = area.center();
    let origin = point(center.x - extent.width / 2., center.y - extent.height / 2.);
    Target {
        output,
        window_bounds: Bounds::new(origin, extent),
        fullscreen: desktop.outputs.len() > 1,
    }
}

fn select_output(desktop: &Desktop, requested: Option<DisplayId>) -> Option<Output> {
    let mut outputs = desktop.outputs.iter().map(|(output, _)| *output);
    outputs
        .clone()
        .find(|output| Some(output.id) == requested)
        .or_else(|| {
            outputs
                .clone()
                .find(|output| Some(output.id) != desktop.presenter)
        })
        .or_else(|| outputs.next())
}

fn desktop(presenter: &Window, cx: &App) -> Desktop {
    #[cfg(target_os = "linux")]
    if let Ok(mut desktop) = x11::desktop(presenter) {
        sort_outputs(&mut desktop.outputs);
        return desktop;
    }

    let mut outputs = cx
        .displays()
        .into_iter()
        .map(|display| {
            (
                Output {
                    id: display.id(),
                    bounds: display.bounds(),
                },
                None,
            )
        })
        .collect::<Vec<_>>();
    sort_outputs(&mut outputs);
    let presenter = presenter
        .display(cx)
        .map(|display| display.id())
        .or_else(|| cx.primary_display().map(|display| display.id()));
    Desktop { outputs, presenter }
}

#[cfg(target_os = "windows")]
fn display_raster_size(id: DisplayId, _: Bounds<Pixels>) -> Option<RasterSize> {
    use windows::{
        Win32::Graphics::Gdi::{
            DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HMONITOR,
            MONITORINFOEXW,
        },
        core::PCWSTR,
    };

    // GPUI's Windows display ID stores the HMONITOR for the currently active output.
    let monitor = HMONITOR(u64::from(id) as _);
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: the initialized MONITORINFO prefix belongs to an entire MONITORINFOEXW
    // buffer and cbSize describes that buffer to GetMonitorInfoW.
    if !unsafe { GetMonitorInfoW(monitor, std::ptr::from_mut(&mut info).cast()) }.as_bool() {
        return None;
    }

    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: GetMonitorInfoW filled the NUL-terminated device name and the
    // initialized DEVMODEW buffer is writable for the lifetime of the call.
    unsafe {
        EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    }
    .as_bool()
    .then_some(RasterSize {
        width: mode.dmPelsWidth,
        height: mode.dmPelsHeight,
    })
}

#[cfg(target_os = "macos")]
fn display_raster_size(id: DisplayId, bounds: Bounds<Pixels>) -> Option<RasterSize> {
    use core_graphics::display::CGDisplay;

    let display = CGDisplay::new(u32::try_from(u64::from(id)).ok()?);
    let mode = display.display_mode()?;
    let mut width = u32::try_from(mode.pixel_width()).ok()?;
    let mut height = u32::try_from(mode.pixel_height()).ok()?;
    let bounds_orientation = bounds.size.width.partial_cmp(&bounds.size.height);
    if bounds_orientation.is_some_and(|orientation| {
        orientation != std::cmp::Ordering::Equal && height.cmp(&width) == orientation
    }) {
        std::mem::swap(&mut width, &mut height);
    }
    Some(RasterSize { width, height })
}

fn sort_outputs(outputs: &mut [(Output, Option<String>)]) {
    outputs.sort_by_key(|(output, _)| {
        (
            output.bounds.origin.x.to_i32(),
            output.bounds.origin.y.to_i32(),
            u64::from(output.id),
        )
    });
}

pub(crate) fn open(
    model: Entity<Presentation>,
    target: Target,
    on_placement_issue: impl FnOnce(&mut App) + 'static,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    // On X11 GPUI display IDs refer to X screens; a RandR monitor ID is a different identifier.
    let display_id = if cfg!(target_os = "linux") {
        None
    } else {
        target.id()
    };
    let (handle, _) = gpui_kit::open_window(
        WindowOptions {
            app_id: Some(crate::APP_ID.into()),
            window_bounds: Some(WindowBounds::Windowed(target.window_bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("スライド画面".into()),
                ..Default::default()
            }),
            display_id,
            focus: false,
            ..Default::default()
        },
        cx,
        |window, cx| cx.new(|cx| Audience::new(model, target, on_placement_issue, window, cx)),
    )?;
    Ok(handle)
}

pub(crate) fn toggle_fullscreen(handle: AnyWindowHandle, cx: &mut App) {
    let _ = handle.update(cx, |_, window, cx| {
        window.dispatch_action(Box::new(ToggleFullscreen), cx);
    });
}

struct AudienceFrame {
    page: DisplayPage,
    canonical: Arc<RenderImage>,
    viewport: Option<(ViewTarget, Option<Arc<RenderImage>>)>,
}

impl AudienceFrame {
    fn capture(model: &Presentation, window: WindowId) -> Option<Self> {
        if model.audience()?.window_id() != window {
            return None;
        }
        let page = model.display_page()?;
        Some(Self {
            page,
            canonical: model.audience_image()?,
            viewport: model
                .audience_view()
                .filter(|(target, _)| target.page == page.stamp),
        })
    }

    fn key(&self) -> (PageStamp, ImageId, Option<ImageId>) {
        let exact = self.viewport.as_ref().and_then(|(_, image)| image.as_ref());
        (
            self.page.stamp,
            self.canonical.id,
            exact.map(|image| image.id),
        )
    }
}

#[derive(Default)]
struct AudienceFrames {
    shown: Option<AudienceFrame>,
    pending: Option<AudienceFrame>,
}

#[derive(Debug, Eq, PartialEq)]
enum FrameUpdate {
    KeepTimer,
    StartTimer,
    ShowNow { redraw: bool },
}

impl AudienceFrames {
    fn update(&mut self, frame: Option<AudienceFrame>) -> (FrameUpdate, Option<AudienceFrame>) {
        if let Some(page) = frame.as_ref().map(|frame| frame.page.stamp) {
            if self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.page.stamp == page)
            {
                self.pending = frame;
                return (FrameUpdate::KeepTimer, None);
            }
            if self.shown.as_ref().is_some_and(|shown| {
                shown.page.stamp.document == page.document && shown.page.stamp.index != page.index
            }) {
                self.pending = frame;
                return (FrameUpdate::StartTimer, None);
            }
        }
        self.pending = None;
        let redraw =
            self.shown.as_ref().map(AudienceFrame::key) != frame.as_ref().map(AudienceFrame::key);
        let previous = std::mem::replace(&mut self.shown, frame);
        (FrameUpdate::ShowNow { redraw }, previous)
    }
}

struct Audience {
    model: Entity<Presentation>,
    focus: FocusHandle,
    show_page: bool,
    frames: AudienceFrames,
    frame_task: Option<Task<()>>,
    _model_subscription: Subscription,
    placement_task: Option<Task<()>>,
}

impl Audience {
    fn new(
        model: Entity<Presentation>,
        target: Target,
        on_placement_issue: impl FnOnce(&mut App) + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let window_handle = window.window_handle();
        let model_subscription = cx.observe(&model, move |view, _, cx| {
            view.sync_frame(window_handle, cx);
        });
        let fullscreen_output = target.output.filter(|_| target.fullscreen);
        let placement_task = fullscreen_output.map(|output| {
            let mut placement = Placement::new(output, target.window_bounds.origin, window);
            cx.spawn_in(window, async move |view, cx| {
                for attempt in 0..PLACEMENT_CHECKS {
                    cx.background_executor().timer(PLACEMENT_INTERVAL).await;
                    let Ok(confirmed) =
                        cx.update(|window, cx| placement.advance(window, cx, attempt))
                    else {
                        return;
                    };
                    if confirmed {
                        let _ = cx.update(|window, cx| {
                            let _ = view.update(cx, |view, cx| {
                                view.show_page = true;
                                view.sync_frame(window.window_handle(), cx);
                            });
                        });
                        return;
                    }
                }

                let _ = cx.update(|window, cx| {
                    let current_window = window.window_handle();
                    let on_target = placement.on_output(window, cx);
                    if !on_target && fullscreen_active(window) {
                        toggle_window_fullscreen(window);
                    }
                    let is_active = view
                        .update(cx, |view, cx| {
                            view.show_page = on_target;
                            view.sync_frame(current_window, cx);
                            view.model.read(cx).audience().is_some_and(|audience| {
                                audience.window_id() == current_window.window_id()
                            })
                        })
                        .unwrap_or(false);
                    if is_active {
                        on_placement_issue(cx);
                    }
                });
            })
        });

        Self {
            model,
            focus,
            show_page: placement_task.is_none(),
            frames: AudienceFrames::default(),
            frame_task: None,
            placement_task,
            _model_subscription: model_subscription,
        }
    }

    fn sync_frame(&mut self, window: AnyWindowHandle, cx: &mut Context<Self>) {
        let frame = self
            .show_page
            .then(|| AudienceFrame::capture(self.model.read(cx), window.window_id()))
            .flatten();
        let (update, previous) = self.frames.update(frame);
        match update {
            FrameUpdate::KeepTimer => {}
            FrameUpdate::StartTimer => {
                self.frame_task = Some(cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(PAGE_SETTLE_DELAY).await;
                    let _ = view.update(cx, |view, cx| {
                        if let Some(task) = view.frame_task.take() {
                            task.detach();
                        }
                        if let Some(frame) = view.frames.pending.take() {
                            let previous = view.frames.shown.replace(frame);
                            view.retire_frame(previous, window, cx);
                            cx.notify();
                        }
                    });
                }));
            }
            FrameUpdate::ShowNow { redraw } => {
                self.frame_task = None;
                if redraw {
                    self.retire_frame(previous, window, cx);
                    cx.notify();
                }
            }
        }
    }

    fn retire_frame(
        &self,
        previous: Option<AudienceFrame>,
        window: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let Some(previous) = previous else { return };
        let shown = self.frames.shown.as_ref().map(AudienceFrame::key);
        let is_shown =
            |id| shown.is_some_and(|(_, fallback, exact)| fallback == id || exact == Some(id));
        let model = self.model.read(cx);
        let retained = model.image(previous.page.stamp.index);
        let is_retained = |id| retained.as_ref().is_some_and(|image| image.id == id);
        let canonical = (!is_shown(previous.canonical.id) && !is_retained(previous.canonical.id))
            .then_some(previous.canonical);
        let exact = previous.viewport.and_then(|(target, image)| {
            let image = image?;
            (!is_shown(image.id)
                && !is_retained(image.id)
                && model
                    .viewport_image(target)
                    .is_none_or(|retained| retained.id != image.id))
            .then_some(image)
        });
        if canonical.is_none() && exact.is_none() {
            return;
        }
        // A window redraw can upload the visible frame again after the model evicts it.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, _| {
                for image in canonical.into_iter().chain(exact) {
                    let _ = window.drop_image(image);
                }
            });
        });
    }

    fn fullscreen_action(
        &mut self,
        _: &ToggleFullscreen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.placement_task = None;
        self.show_page = true;
        toggle_window_fullscreen(window);
        self.sync_frame(window.window_handle(), cx);
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for Audience {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = SlideViewport::Audience(window.window_handle().window_id());
        let slide = self
            .frames
            .shown
            .as_ref()
            .map(|frame| audience_canvas(cx.weak_entity(), viewport, frame));

        div()
            .id("audience-slide")
            .key_context("Audience")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &NextSlide, _, cx| {
                this.model
                    .update(cx, |model, cx| model.navigate_relative(1, cx));
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &PreviousSlide, _, cx| {
                this.model
                    .update(cx, |model, cx| model.navigate_relative(-1, cx));
                cx.stop_propagation();
            }))
            .on_action(|_: &EndPresentation, window, cx| {
                window.remove_window();
                cx.stop_propagation();
            })
            .on_action(cx.listener(Self::fullscreen_action))
            .size_full()
            .overflow_hidden()
            .bg(rgb(0x000000))
            .children(slide)
    }
}

fn audience_canvas(
    audience: WeakEntity<Audience>,
    viewport: SlideViewport,
    frame: &AudienceFrame,
) -> impl IntoElement + use<> {
    let page = frame.page;
    let canonical = frame.canonical.clone();
    let raster = frame.viewport.clone();
    canvas(
        move |bounds, window, cx| {
            let scale = window.scale_factor();
            let placement = page_placement(bounds, scale, page.geometry, SlideFit::WholePage);
            let request = placement
                .filter(|placement| {
                    placement
                        .raster_size
                        .width
                        .max(placement.raster_size.height)
                        <= MAX_TEXTURE_EDGE
                })
                .map(|placement| ViewRequest {
                    target: ViewTarget {
                        page: page.stamp,
                        pixels: placement.raster_size,
                    },
                    render_bounds: placement.render_bounds,
                });
            let target = request.map(|request| request.target);
            let known_target = raster.as_ref().map(|(target, _)| *target);
            // GPUI tracks entity reads during drawing. Keep the shared model out of the draw path.
            if target != known_target {
                window.defer(cx, move |window, cx| {
                    let _ = audience.update(cx, |audience, cx| {
                        audience.model.update(cx, |model, cx| {
                            if let Some(request) = request {
                                model.request_viewport(viewport, request, cx);
                            } else if model
                                .display_page()
                                .is_some_and(|shown| shown.stamp == page.stamp)
                            {
                                model.clear_viewport(viewport, cx);
                            }
                        });
                        audience.sync_frame(window.window_handle(), cx);
                    });
                });
            }
            let image_bounds = match placement {
                Some(placement) => placement.bounds,
                None => fitted_page(bounds, scale, raster_size(&canonical))?.0,
            };
            let exact = raster
                .filter(|(known, _)| Some(*known) == target)
                .and_then(|(_, image)| image);
            Some((image_bounds, canonical, exact))
        },
        |_, slide, window, _| {
            if let Some((bounds, canonical, exact)) = slide
                && !exact.is_some_and(|image| paint_page(bounds, bounds, image, window))
            {
                paint_page(bounds, bounds, canonical, window);
            }
        },
    )
    .size_full()
}

fn toggle_window_fullscreen(window: &Window) {
    #[cfg(target_os = "macos")]
    if !window.is_fullscreen() {
        // Keep the presenter and audience on the same set of macOS Spaces.
        window.toggle_simple_fullscreen();
        return;
    }
    window.toggle_fullscreen();
}

fn fullscreen_active(window: &Window) -> bool {
    window.is_fullscreen() || window.is_simple_fullscreen()
}

struct Placement {
    output: Output,
    requested_fullscreen: bool,
    #[cfg(target_os = "linux")]
    x11: Option<x11::Placement>,
}

impl Placement {
    fn new(output: Output, origin: Point<Pixels>, window: &Window) -> Self {
        #[cfg(not(target_os = "linux"))]
        let _ = (origin, window);
        Self {
            output,
            requested_fullscreen: false,
            #[cfg(target_os = "linux")]
            x11: x11::Placement::new(origin, window).ok(),
        }
    }

    fn advance(&mut self, window: &Window, cx: &App, attempt: usize) -> bool {
        if !self.on_output(window, cx) {
            #[cfg(target_os = "linux")]
            if attempt == 1
                && let Some(x11) = &self.x11
            {
                let _ = x11.request_move();
            }
            #[cfg(not(target_os = "linux"))]
            let _ = attempt;
            return false;
        }
        if fullscreen_active(window) {
            return true;
        }
        if !self.requested_fullscreen {
            toggle_window_fullscreen(window);
            self.requested_fullscreen = true;
        }
        false
    }

    fn on_output(&self, window: &Window, cx: &App) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(x11) = &self.x11 {
            return x11
                .window_center()
                .is_ok_and(|center| self.output.bounds.contains(&center));
        }
        window
            .display(cx)
            .is_some_and(|display| display.id() == self.output.id)
    }
}

#[cfg(target_os = "linux")]
mod x11 {
    use gpui_kit::{Bounds, DisplayId, Pixels, Point, Result, Window, point, px, size};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::{
        connection::Connection,
        protocol::{
            randr::{ConnectionExt as _, MonitorInfo},
            xproto::{ClientMessageEvent, ConnectionExt as _, EventMask},
        },
        rust_connection::RustConnection,
    };

    use super::{Desktop, Output, RasterSize};

    pub(super) fn raster_sizes() -> Result<Vec<RasterSize>> {
        let (connection, root) = desktop_connection()?;
        Ok(active_monitors(&connection, root)?
            .into_iter()
            .map(|monitor| RasterSize {
                width: u32::from(monitor.width),
                height: u32::from(monitor.height),
            })
            .collect())
    }

    pub(super) fn desktop(presenter: &Window) -> Result<Desktop> {
        let (connection, root) = desktop_connection()?;
        let scale = presenter.scale_factor();
        let center = window_center(&connection, root, window_id(presenter)?, scale)
            .unwrap_or_else(|_| presenter.bounds().center());
        let monitors = active_monitors(&connection, root)?;
        let mut presenter_monitor = None;
        let mut primary_monitor = None;
        let mut outputs = Vec::with_capacity(monitors.len());
        for monitor in monitors {
            if monitor.width == 0 || monitor.height == 0 {
                continue;
            }
            let id = DisplayId::new(u64::from(monitor.name));
            let bounds = Bounds::new(
                point(
                    px(f32::from(monitor.x) / scale),
                    px(f32::from(monitor.y) / scale),
                ),
                size(
                    px(f32::from(monitor.width) / scale),
                    px(f32::from(monitor.height) / scale),
                ),
            );
            if bounds.contains(&center) {
                presenter_monitor = Some(id);
            }
            if monitor.primary {
                primary_monitor = Some(id);
            }
            let label = connection
                .get_atom_name(monitor.name)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .and_then(|reply| String::from_utf8(reply.name).ok());
            outputs.push((Output { id, bounds }, label));
        }
        let presenter = presenter_monitor
            .or(primary_monitor)
            .or_else(|| outputs.first().map(|(output, _)| output.id));
        Ok(Desktop { outputs, presenter })
    }

    fn desktop_connection() -> Result<(RustConnection, u32)> {
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        Ok((connection, root))
    }

    fn active_monitors(connection: &RustConnection, root: u32) -> Result<Vec<MonitorInfo>> {
        Ok(connection.randr_get_monitors(root, true)?.reply()?.monitors)
    }

    pub(super) struct Placement {
        connection: RustConnection,
        root: u32,
        window: u32,
        origin: Point<Pixels>,
        scale: f32,
    }

    impl Placement {
        pub(super) fn new(origin: Point<Pixels>, window: &Window) -> Result<Self> {
            let (connection, root) = desktop_connection()?;
            Ok(Self {
                connection,
                root,
                window: window_id(window)?,
                origin,
                scale: window.scale_factor(),
            })
        }

        pub(super) fn window_center(&self) -> Result<Point<Pixels>> {
            window_center(&self.connection, self.root, self.window, self.scale)
        }

        pub(super) fn request_move(&self) -> Result<()> {
            let atom = self
                .connection
                .intern_atom(true, b"_NET_MOVERESIZE_WINDOW")?
                .reply()?
                .atom;
            if atom == 0 {
                return Ok(());
            }
            let x = (self.origin.x.as_f32() * self.scale).round() as i32;
            let y = (self.origin.y.as_f32() * self.scale).round() as i32;
            // Static gravity, x/y fields present, source is this application (EWMH).
            let flags = 10 | (1 << 8) | (1 << 9) | (1 << 12);
            self.connection
                .send_event(
                    false,
                    self.root,
                    EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                    ClientMessageEvent::new(
                        32,
                        self.window,
                        atom,
                        [flags, x as u32, y as u32, 0, 0],
                    ),
                )?
                .check()?;
            self.connection.flush()?;
            Ok(())
        }
    }

    fn window_id(window: &Window) -> Result<u32> {
        match HasWindowHandle::window_handle(window)?.as_raw() {
            RawWindowHandle::Xcb(handle) => Ok(handle.window.get()),
            RawWindowHandle::Xlib(handle) => Ok(u32::try_from(handle.window)?),
            _ => anyhow::bail!("スライド画面の X11 ウィンドウを取得できない"),
        }
    }

    fn window_center(
        connection: &RustConnection,
        root: u32,
        window: u32,
        scale: f32,
    ) -> Result<Point<Pixels>> {
        let geometry = connection.get_geometry(window)?.reply()?;
        let center = connection
            .translate_coordinates(
                window,
                root,
                (geometry.width / 2) as i16,
                (geometry.height / 2) as i16,
            )?
            .reply()?;
        Ok(point(
            px(f32::from(center.dst_x) / scale),
            px(f32::from(center.dst_y) / scale),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::PageSize;

    fn frame(document: u64, index: usize) -> AudienceFrame {
        AudienceFrame {
            page: DisplayPage {
                stamp: PageStamp {
                    document,
                    generation: 1,
                    index,
                },
                geometry: PageSize {
                    width: 1.0,
                    height: 1.0,
                },
            },
            canonical: Arc::new(RenderImage::new([image::Frame::new(
                image::RgbaImage::new(1, 1),
            )])),
            viewport: None,
        }
    }

    #[test]
    fn keeps_visible_page_and_coalesces_newer_frames() {
        let mut frames = AudienceFrames::default();
        assert_eq!(
            frames.update(Some(frame(1, 0))).0,
            FrameUpdate::ShowNow { redraw: true }
        );
        assert_eq!(frames.update(Some(frame(1, 1))).0, FrameUpdate::StartTimer);
        assert_eq!(frames.update(Some(frame(1, 2))).0, FrameUpdate::StartTimer);

        let newer = frame(1, 2);
        let key = newer.key();
        assert_eq!(frames.update(Some(newer)).0, FrameUpdate::KeepTimer);
        assert_eq!(frames.shown.unwrap().page.stamp.index, 0);
        assert_eq!(frames.pending.unwrap().key(), key);
    }

    #[test]
    fn returning_to_visible_page_clears_the_pending_frame() {
        let mut frames = AudienceFrames::default();
        let current = frame(1, 0);
        frames.update(Some(AudienceFrame {
            page: current.page,
            canonical: current.canonical.clone(),
            viewport: None,
        }));
        frames.update(Some(frame(1, 1)));
        assert_eq!(
            frames.update(Some(current)).0,
            FrameUpdate::ShowNow { redraw: false }
        );
        assert!(frames.pending.is_none());
    }

    #[test]
    fn applies_exact_image_and_document_changes_immediately() {
        let mut frames = AudienceFrames::default();
        let mut current = frame(1, 0);
        frames.update(Some(AudienceFrame {
            page: current.page,
            canonical: current.canonical.clone(),
            viewport: None,
        }));
        current.viewport = Some((
            ViewTarget {
                page: current.page.stamp,
                pixels: RasterSize {
                    width: 1,
                    height: 1,
                },
            },
            Some(frame(1, 0).canonical),
        ));
        assert_eq!(
            frames.update(Some(current)).0,
            FrameUpdate::ShowNow { redraw: true }
        );
        assert_eq!(frames.update(Some(frame(1, 1))).0, FrameUpdate::StartTimer);
        assert_eq!(
            frames.update(Some(frame(2, 0))).0,
            FrameUpdate::ShowNow { redraw: true }
        );
        assert!(frames.pending.is_none());
    }

    #[test]
    fn selects_an_actual_display_by_pixel_area() {
        let wide = RasterSize {
            width: 7680,
            height: 1080,
        };
        let portrait = RasterSize {
            width: 2160,
            height: 4320,
        };
        let landscape = RasterSize {
            width: 3840,
            height: 2160,
        };
        let largest = select_largest_raster_size([wide, portrait, landscape]);
        assert_eq!((largest.width, largest.height), (2160, 4320));
    }

    #[test]
    fn uses_fallback_only_if_no_valid_mode_is_available() {
        let fallback = select_largest_raster_size([RasterSize {
            width: 4096,
            height: 0,
        }]);
        assert_eq!((fallback.width, fallback.height), (1920, 1080));
        let smaller = select_largest_raster_size([RasterSize {
            width: 1280,
            height: 800,
        }]);
        assert!(smaller.width < FALLBACK_RASTER_SIZE.width);
        assert_eq!((smaller.width, smaller.height), (1280, 800));
    }
}
