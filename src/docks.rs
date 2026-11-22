use std::{cell::Cell, ops::Range, rc::Rc};

use gpui_kit::{
    component::{
        ActiveTheme as _, Disableable as _, IconName, Sizable as _, VirtualListScrollHandle,
        WindowExt as _,
        button::{Button, ButtonVariants as _},
        dialog::{DialogAction, DialogClose, DialogFooter},
        dock::{
            BasePanel, DockArea, DockAreaState, DockEvent, DockLayout, DockSkin, Panel, PanelEvent,
            PanelId, PanelInfo, PanelState, panel_handle, register_panel,
        },
        h_virtual_list,
        input::{Input, InputState},
        menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
        v_virtual_list,
    },
    prelude::FluentBuilder as _,
    *,
};

use crate::{
    actions::OpenPdf,
    presentation::Presentation,
    settings::{
        MAX_COUNTDOWN_SECONDS, SlideFit, SlideListLayout, SlideListMode, SlideListSettings,
        ThumbnailSize, TimerMode,
    },
    slide_view::{fitted_page, paint_page, raster_size},
    timer::TimerState,
    view_raster::{SlideViewport, slide_canvas},
};

const LAYOUT_VERSION: usize = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PanelKind {
    Timer,
    Slides,
    Current,
}

impl PanelKind {
    pub(crate) const ALL: [Self; 3] = [Self::Timer, Self::Slides, Self::Current];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Timer => "カウンター",
            Self::Slides => "スライド一覧",
            Self::Current => "現在スライド",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Timer => "elafra.timer",
            Self::Slides => "elafra.slides",
            Self::Current => "elafra.current-slide",
        }
    }
}

pub(crate) struct WorkspaceDocks {
    pub(crate) area: Entity<DockArea>,
    panels: [Entity<DockPanel>; 3],
    _skin: Rc<DockSkin>,
}

impl WorkspaceDocks {
    pub(crate) fn new(
        model: Entity<Presentation>,
        saved_layout: Option<serde_json::Value>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let (area, skin) = DockSkin::dock_area("presenter-docks", Some(LAYOUT_VERSION), window, cx);
        let panels = PanelKind::ALL
            .map(|kind| cx.new(|cx| DockPanel::new(kind, model.clone(), area.downgrade(), cx)));
        for (kind, panel) in PanelKind::ALL.into_iter().zip(panels.iter().cloned()) {
            register_panel(cx, kind.name(), move |state, _, cx| {
                let hidden = matches!(state.info(), PanelInfo::Panel(value) if value.get("hidden") == Some(&serde_json::Value::Bool(true)));
                panel.update(cx, |panel, _| panel.hidden = hidden);
                panel_handle(panel.clone())
            });
        }

        let saved = saved_layout
            .and_then(|state| serde_json::from_value::<DockAreaState>(state).ok())
            .filter(valid_layout);
        if !saved
            .is_some_and(|state| area.update(cx, |area, cx| area.load(state, window, cx).is_ok()))
        {
            let tab = |kind: PanelKind| {
                DockLayout::tabs().panel_view(panel_handle(panels[kind as usize].clone()), cx)
            };
            let layout = DockLayout::h_split()
                .child(tab(PanelKind::Slides), Some(px(208.)))
                .child(tab(PanelKind::Current), None)
                .child(tab(PanelKind::Timer), Some(px(220.)));
            area.update(cx, |area, cx| area.set_center(layout, window, cx));
        }

        Self {
            area,
            panels,
            _skin: skin,
        }
    }

    pub(crate) fn show(&self, kind: PanelKind, window: &mut Window, cx: &mut App) {
        let panel = &self.panels[kind as usize];
        let was_hidden = panel.update(cx, |panel, cx| {
            let was_hidden = panel.hidden;
            if was_hidden {
                panel.hidden = false;
                cx.notify();
            }
            was_hidden
        });
        let id = PanelId::from(panel.entity_id());
        self.area.update(cx, |area, cx| {
            area.select_panel(id, window, cx);
            if was_hidden {
                cx.emit(DockEvent::LayoutChanged);
                cx.notify();
            }
        });
        window.refresh();
    }

    pub(crate) fn layout(&self, cx: &App) -> serde_json::Value {
        serde_json::to_value(self.area.read(cx).dump(cx)).expect("serializable dock state")
    }
}

fn valid_layout(state: &DockAreaState) -> bool {
    fn walk(state: &PanelState, seen: &mut [bool; 3]) -> bool {
        match &state.info {
            PanelInfo::Stack { axis, .. } => {
                state.panel_name == "StackPanel"
                    && *axis < 2
                    && state.children.iter().all(|child| walk(child, seen))
            }
            PanelInfo::Tabs { active_index } => {
                state.panel_name == "TabPanel"
                    && *active_index < state.children.len().max(1)
                    && state.children.iter().all(|child| walk(child, seen))
            }
            PanelInfo::Panel(_) => PanelKind::ALL
                .iter()
                .find(|kind| kind.name() == state.panel_name)
                .is_some_and(|kind| {
                    let slot = &mut seen[*kind as usize];
                    state.children.is_empty() && !std::mem::replace(slot, true)
                }),
        }
    }

    if state.version != Some(LAYOUT_VERSION)
        || state.left_dock.is_some()
        || state.right_dock.is_some()
        || state.bottom_dock.is_some()
    {
        return false;
    }
    let mut seen = [false; 3];
    walk(&state.center, &mut seen) && seen.into_iter().all(|found| found)
}

struct DockPanel {
    kind: PanelKind,
    model: Entity<Presentation>,
    area: WeakEntity<DockArea>,
    focus: FocusHandle,
    hidden: bool,
    viewport: DockViewport,
    vertical_scroll: VirtualListScrollHandle,
    horizontal_scroll: VirtualListScrollHandle,
    slide_list: Option<SlideList>,
    _model_subscription: Subscription,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DockViewport {
    width: u32,
    height: u32,
}

impl DockViewport {
    fn inline_controls(self) -> bool {
        self.height < 132 && self.width >= 224
    }
}

const CARD_PADDING: u32 = 16;
const CARD_LABEL_SPACE: u32 = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SlideListGeometry {
    viewport: DockViewport,
    direction: SlideListLayout,
    page_count: usize,
    columns: usize,
    card_width: u32,
    card_height: u32,
    thumbnail_width: Option<u32>,
}

impl SlideListGeometry {
    fn new(viewport: DockViewport, settings: SlideListSettings, page_count: usize) -> Self {
        let preferred_width = match settings.size {
            ThumbnailSize::Small => 96,
            ThumbnailSize::Medium => 144,
            ThumbnailSize::Large => 192,
        };
        let thumbnail_width = preferred_width
            .min(viewport.width.saturating_sub(CARD_PADDING))
            .min(
                viewport
                    .height
                    .saturating_sub(CARD_LABEL_SPACE)
                    .saturating_mul(4)
                    / 3,
            );
        let thumbnail_width = (settings.mode == SlideListMode::Thumbnails && thumbnail_width >= 36)
            .then_some(thumbnail_width);
        let card_width = thumbnail_width
            .map_or(72, |width| width + CARD_PADDING)
            .min(viewport.width.max(1));
        let columns = if settings.layout == SlideListLayout::Grid {
            ((viewport.width / card_width) as usize).clamp(1, page_count.max(1))
        } else {
            1
        };

        Self {
            viewport,
            direction: settings.layout,
            page_count,
            columns,
            card_width,
            card_height: thumbnail_width
                .map_or(CARD_LABEL_SPACE, |width| width * 3 / 4 + CARD_LABEL_SPACE)
                .min(viewport.height.max(1)),
            thumbnail_width,
        }
    }

    fn horizontal(self) -> bool {
        self.direction == SlideListLayout::Horizontal
    }

    fn rows(self) -> usize {
        self.page_count.div_ceil(self.columns)
    }

    fn item_extent(self) -> u32 {
        if self.horizontal() {
            self.card_width
        } else {
            self.card_height
        }
    }

    fn viewport_extent(self) -> u32 {
        if self.horizontal() {
            self.viewport.width
        } else {
            self.viewport.height
        }
    }

    fn edge_space(self) -> f32 {
        self.viewport_extent().saturating_sub(self.item_extent()) as f32 / 2.0
    }

    fn item_sizes(self) -> Vec<Size<Pixels>> {
        let (card, spacer) = if self.horizontal() {
            (
                size(px(self.card_width as f32), px(self.viewport.height as f32)),
                size(px(self.edge_space()), px(self.viewport.height as f32)),
            )
        } else {
            (
                size(px(self.viewport.width as f32), px(self.card_height as f32)),
                size(px(self.viewport.width as f32), px(self.edge_space())),
            )
        };
        let mut sizes = vec![card; self.rows() + 2];
        sizes[0] = spacer;
        sizes[self.rows() + 1] = spacer;
        sizes
    }

    fn item_for_page(self, page: usize) -> usize {
        1 + page / self.columns
    }

    fn pages_in_item(self, item: usize) -> Range<usize> {
        if item == 0 || item > self.rows() {
            return 0..0;
        }
        let start = (item - 1) * self.columns;
        start..(start + self.columns).min(self.page_count)
    }

    fn visible_pages(self, offset: Pixels, extent: Pixels) -> Range<usize> {
        if self.thumbnail_width.is_none() || extent <= px(0.0) {
            return 0..0;
        }
        let extent_per_row = self.item_extent() as f32;
        let start = -offset.as_f32() - self.edge_space();
        let first = (start / extent_per_row).floor() as usize;
        let end = ((start + extent.as_f32()) / extent_per_row).ceil() as usize;
        let to_page = |row: usize| (row.min(self.rows()) * self.columns).min(self.page_count);
        to_page(first)..to_page(end)
    }
}

struct SlideList {
    geometry: SlideListGeometry,
    sizes: Rc<Vec<Size<Pixels>>>,
    center: Option<(u64, usize)>,
}

impl SlideList {
    fn new(geometry: SlideListGeometry) -> Self {
        Self {
            geometry,
            sizes: Rc::new(geometry.item_sizes()),
            center: None,
        }
    }

    fn center_on(&mut self, document_id: u64, page: usize) -> Option<usize> {
        let center = (document_id, page);
        (self.center.replace(center) != Some(center)).then(|| self.geometry.item_for_page(page))
    }
}

impl DockPanel {
    fn new(
        kind: PanelKind,
        model: Entity<Presentation>,
        area: WeakEntity<DockArea>,
        cx: &mut Context<Self>,
    ) -> Self {
        let model_subscription = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            kind,
            model,
            area,
            focus: cx.focus_handle(),
            hidden: false,
            viewport: DockViewport {
                width: 220,
                height: 300,
            },
            vertical_scroll: VirtualListScrollHandle::new(),
            horizontal_scroll: VirtualListScrollHandle::new(),
            slide_list: None,
            _model_subscription: model_subscription,
        }
    }

    fn menu(
        panel: WeakEntity<Self>,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(handle) = panel.upgrade() else {
            return menu;
        };
        let (kind, model, area) = {
            let panel = handle.read(cx);
            (panel.kind, panel.model.clone(), panel.area.clone())
        };
        let menu = match kind {
            PanelKind::Timer => Self::timer_menu(&model, menu, cx),
            PanelKind::Slides => Self::slides_menu(&model, menu, window, cx),
            PanelKind::Current => Self::current_menu(&model, menu, cx),
        };
        menu.separator()
            .item(
                PopupMenuItem::new("このドックを非表示").on_click(move |_, window, cx| {
                    _ = panel.update(cx, |panel, cx| {
                        panel.hidden = true;
                        panel.clear_panel_requests(cx);
                        cx.notify();
                    });
                    _ = area.update(cx, |_, cx| {
                        cx.emit(DockEvent::LayoutChanged);
                        cx.notify();
                    });
                    window.refresh();
                }),
            )
    }

    fn timer_menu(model: &Entity<Presentation>, mut menu: PopupMenu, cx: &App) -> PopupMenu {
        let settings = model.read(cx).settings().timer;
        for (mode, label) in [
            (TimerMode::CountUp, "カウントアップ"),
            (TimerMode::CountDown, "カウントダウン"),
        ] {
            let model = model.clone();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .checked(mode == settings.mode)
                    .on_click(move |_, _, cx| {
                        model.update(cx, |model, cx| {
                            let mut settings = model.settings().timer;
                            settings.mode = mode;
                            model.set_timer_settings(settings, cx);
                        });
                    }),
            );
        }
        let model = model.clone();
        menu.separator()
            .item(
                PopupMenuItem::new("カウントダウン時間を設定…").on_click(move |_, window, cx| {
                    let model = model.clone();
                    window.defer(cx, move |window, cx| edit_countdown(model, window, cx));
                }),
            )
    }

    fn slides_menu(
        model: &Entity<Presentation>,
        mut menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let settings = model.read(cx).settings().slides;
        for (mode, label) in [
            (SlideListMode::Thumbnails, "サムネイル"),
            (SlideListMode::PageNumbers, "ページ番号のみ"),
        ] {
            let model = model.clone();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .checked(mode == settings.mode)
                    .on_click(move |_, _, cx| {
                        model.update(cx, |model, cx| {
                            let mut settings = model.settings().slides;
                            settings.mode = mode;
                            model.set_slide_list_settings(settings, cx);
                            if mode == SlideListMode::PageNumbers {
                                model.set_visible_thumbnails(Vec::new(), cx);
                            }
                        });
                    }),
            );
        }
        let layout_model = model.clone();
        let menu = menu.submenu("並び方", window, cx, move |mut menu, _, _| {
            for (layout, label) in [
                (SlideListLayout::Vertical, "縦並び"),
                (SlideListLayout::Horizontal, "横並び"),
                (SlideListLayout::Grid, "グリッド"),
            ] {
                let model = layout_model.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(layout == settings.layout)
                        .on_click(move |_, _, cx| {
                            model.update(cx, |model, cx| {
                                let mut settings = model.settings().slides;
                                settings.layout = layout;
                                model.set_slide_list_settings(settings, cx);
                            });
                        }),
                );
            }
            menu
        });
        let model = model.clone();
        menu.submenu(
            "サムネイルの大きさ",
            window,
            cx,
            move |mut menu, _, _| {
                for (size, label) in [
                    (ThumbnailSize::Small, "小"),
                    (ThumbnailSize::Medium, "中"),
                    (ThumbnailSize::Large, "大"),
                ] {
                    let model = model.clone();
                    menu = menu.item(
                        PopupMenuItem::new(label)
                            .checked(size == settings.size)
                            .on_click(move |_, _, cx| {
                                model.update(cx, |model, cx| {
                                    let mut settings = model.settings().slides;
                                    settings.size = size;
                                    model.set_slide_list_settings(settings, cx);
                                });
                            }),
                    );
                }
                menu
            },
        )
    }

    fn current_menu(model: &Entity<Presentation>, mut menu: PopupMenu, cx: &App) -> PopupMenu {
        let settings = model.read(cx).settings().current_slide;
        for (fit, label) in [
            (SlideFit::WholePage, "全体表示"),
            (SlideFit::FitWidth, "横幅に合わせる"),
        ] {
            let model = model.clone();
            menu = menu.item(
                PopupMenuItem::new(label)
                    .checked(fit == settings.fit)
                    .on_click(move |_, _, cx| {
                        model.update(cx, |model, cx| {
                            let mut settings = model.settings().current_slide;
                            settings.fit = fit;
                            model.set_current_slide_settings(settings, cx);
                        });
                    }),
            );
        }
        menu
    }

    fn clear_panel_requests(&self, cx: &mut Context<Self>) {
        match self.kind {
            PanelKind::Slides => self.model.update(cx, |model, cx| {
                model.set_visible_thumbnails(Vec::new(), cx);
            }),
            PanelKind::Current => self.model.update(cx, |model, cx| {
                model.clear_viewport(SlideViewport::CurrentSlide, cx);
            }),
            PanelKind::Timer => {}
        }
    }

    fn timer(&self, cx: &App) -> AnyElement {
        let presentation = self.model.read(cx);
        let snapshot = presentation.timer_snapshot();
        let projecting = presentation.is_presenting();
        let mode = match presentation.settings().timer.mode {
            TimerMode::CountUp => "カウントアップ",
            TimerMode::CountDown => "カウントダウン",
        };
        let status = match snapshot.state {
            TimerState::Ready => "待機中",
            TimerState::Running => "計測中",
            TimerState::Paused => "一時停止中",
            TimerState::Expired => "時間終了",
        };
        let (control, control_icon) = match snapshot.state {
            TimerState::Running => ("一時停止", IconName::Pause),
            TimerState::Paused => ("再開", IconName::Play),
            TimerState::Ready | TimerState::Expired => ("開始", IconName::Play),
        };
        let color = if snapshot.state == TimerState::Expired {
            cx.theme().danger
        } else {
            cx.theme().foreground
        };
        let model = self.model.clone();
        let reset_model = self.model.clone();
        let inline = self.viewport.inline_controls();
        let icon_controls = self.viewport.width < if inline { 336 } else { 144 };
        let show_metadata = !inline || (self.viewport.width >= 380 && self.viewport.height >= 48);
        let reserved_width = if inline {
            let controls = if icon_controls { 64 } else { 144 };
            controls + if show_metadata { 108 } else { 0 }
        } else {
            0
        };
        let timestamp = snapshot.formatted();
        let text_width = self.viewport.width.saturating_sub(16 + reserved_width);
        let timer_font = (text_width as f32 / (timestamp.len() as f32 * 0.64)).clamp(11., 30.);
        let muted = cx.theme().muted_foreground;
        let controls = div()
            .flex()
            .flex_wrap()
            .justify_center()
            .max_w_full()
            .gap_1()
            .when(inline, |controls| controls.flex_none())
            .child(
                Button::new("timer-toggle")
                    .when(icon_controls, |button| {
                        button
                            .icon(control_icon)
                            .accessibility_label(control)
                            .tooltip(control)
                    })
                    .when(!icon_controls, |button| button.label(control))
                    .primary()
                    .small()
                    .disabled(!projecting || snapshot.state == TimerState::Expired)
                    .on_click(move |_, _, cx| {
                        model.update(cx, |model, cx| {
                            if model.timer_snapshot().state == TimerState::Running {
                                model.timer_pause(cx);
                            } else {
                                model.timer_start(cx);
                            }
                        });
                    }),
            )
            .child(
                Button::new("timer-reset")
                    .when(icon_controls, |button| {
                        button
                            .icon(IconName::Undo)
                            .accessibility_label("リセット")
                            .tooltip("リセット")
                    })
                    .when(!icon_controls, |button| button.label("リセット"))
                    .outline()
                    .small()
                    .disabled(snapshot.state == TimerState::Ready)
                    .on_click(move |_, _, cx| {
                        reset_model.update(cx, |model, cx| model.timer_reset(cx));
                    }),
            );
        div()
            .id("timer-scroll")
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .child(
                div()
                    .w_full()
                    .min_h_full()
                    .flex()
                    .when(!inline, |body| body.flex_col())
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .p_2()
                    .child(
                        div()
                            .min_w_0()
                            .max_w_full()
                            .text_size(px(timer_font))
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_color(color)
                            .truncate()
                            .child(timestamp),
                    )
                    .when(show_metadata, |body| {
                        body.child(
                            div()
                                .min_w_0()
                                .max_w_full()
                                .text_xs()
                                .text_center()
                                .text_color(muted)
                                .child(div().child(mode))
                                .child(div().child(status)),
                        )
                    })
                    .child(controls),
            )
            .into_any_element()
    }

    fn slides(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let presentation = self.model.read(cx);
        let Some(document_id) = presentation.document_id() else {
            self.slide_list = None;
            return panel_message("PDFが未読込", cx).into_any_element();
        };
        let geometry = SlideListGeometry::new(
            self.viewport,
            presentation.settings().slides,
            presentation.page_count(),
        );
        let selected = presentation.presenter_page_index();
        if self
            .slide_list
            .as_ref()
            .is_none_or(|list| list.geometry != geometry)
        {
            self.slide_list = Some(SlideList::new(geometry));
        }
        let state = self.slide_list.as_mut().expect("slide list is initialized");
        let scroll = if geometry.horizontal() {
            self.horizontal_scroll.clone()
        } else {
            self.vertical_scroll.clone()
        };
        if let Some(item) = state.center_on(document_id, selected) {
            scroll.scroll_to_item(item, ScrollStrategy::Center);
        }
        let sizes = state.sizes.clone();
        let render_items =
            move |panel: &mut Self, range: Range<usize>, _: &mut Window, cx: &mut Context<Self>| {
                range
                    .map(|item| slide_list_item(&panel.model, geometry, item, selected, cx))
                    .collect()
            };
        let list = if geometry.horizontal() {
            let mut list =
                h_virtual_list(cx.entity(), "slide-list-horizontal", sizes, render_items);
            // GPUI routes a vertical wheel delta to the only scrollable axis.
            list.style().restrict_scroll_to_axis = Some(false);
            list.overflow_y_hidden()
        } else {
            let id = if geometry.direction == SlideListLayout::Grid {
                "slide-list-grid"
            } else {
                "slide-list-vertical"
            };
            v_virtual_list(cx.entity(), id, sizes, render_items).overflow_x_hidden()
        }
        .track_scroll(&scroll)
        .with_item_to_measure_index(1)
        .size_full();

        let model = self.model.clone();
        let visible = canvas(
            move |bounds, window, cx| {
                let offset = scroll.offset();
                let (offset, extent) = if geometry.horizontal() {
                    (offset.x, bounds.size.width)
                } else {
                    (offset.y, bounds.size.height)
                };
                let pages = geometry.visible_pages(offset, extent).collect();
                window.defer(cx, move |_, cx| {
                    model.update(cx, |model, cx| {
                        if model.document_id() == Some(document_id) {
                            model.set_visible_thumbnails(pages, cx);
                        }
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();

        div()
            .relative()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(list)
            .child(visible)
            .into_any_element()
    }

    fn current(&self, cx: &App) -> AnyElement {
        let presentation = self.model.read(cx);
        if presentation.page_count() == 0 {
            return self.empty_current(cx);
        }

        let index = presentation.presenter_page_index();
        let fit = presentation.settings().current_slide.fit;
        let Some((page, image)) = presentation
            .presenter_display_page()
            .zip(presentation.presenter_image())
        else {
            let message = if presentation.page_error(index) {
                "ページを描画できない"
            } else {
                "描画中…"
            };
            return panel_message(message, cx).into_any_element();
        };
        let backdrop = rgb(0x16191f);
        let padding = if self.viewport.width < 120 || self.viewport.height < 120 {
            px(4.)
        } else {
            px(12.)
        };
        let slide = slide_canvas(
            self.model.clone(),
            SlideViewport::CurrentSlide,
            page,
            image,
            fit,
        );

        match fit {
            SlideFit::WholePage => div()
                .size_full()
                .min_h_0()
                .min_w_0()
                .bg(backdrop)
                .p(padding)
                .child(slide)
                .into_any_element(),
            SlideFit::FitWidth => div()
                .id(("wide-slide", index))
                .size_full()
                .min_h_0()
                .min_w_0()
                .overflow_x_hidden()
                .overflow_y_scroll()
                .bg(backdrop)
                .p(padding)
                .child(
                    div()
                        .relative()
                        .w_full()
                        .aspect_ratio((page.geometry.width / page.geometry.height) as f32)
                        .child(div().absolute().inset_0().child(slide)),
                )
                .into_any_element(),
        }
    }

    fn empty_current(&self, cx: &App) -> AnyElement {
        let inline = self.viewport.inline_controls();
        let icon_only = self.viewport.width < 112;
        let focus = self.focus.clone();
        let open = Button::new("open-pdf-from-current-slide")
            .small()
            .outline()
            .when(icon_only, |button| {
                button
                    .icon(IconName::FolderOpen)
                    .accessibility_label("ファイルを開く")
                    .tooltip("ファイルを開く")
            })
            .when(!icon_only, |button| button.label("ファイルを開く"))
            .on_click(move |_, window, cx| focus.dispatch_action(&OpenPdf, window, cx));
        let hint = div()
            .min_w_0()
            .max_w_full()
            .text_sm()
            .text_center()
            .text_color(cx.theme().muted_foreground)
            .child("PDFをここにドロップ");
        let content = div()
            .w_full()
            .min_h_full()
            .flex()
            .when(!inline, |body| body.flex_col())
            .items_center()
            .justify_center()
            .gap_2()
            .p_2();
        let content = if !inline && self.viewport.height < 88 {
            content.child(open).child(hint)
        } else {
            content.child(hint).child(open)
        };
        div()
            .id("empty-current-slide-scroll")
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_x_hidden()
            .overflow_y_scroll()
            .child(content)
            .into_any_element()
    }
}

impl Focusable for DockPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<PanelEvent> for DockPanel {}

impl BasePanel for DockPanel {
    fn panel_name(&self) -> &'static str {
        self.kind.name()
    }

    fn visible(&self, _: &App) -> bool {
        !self.hidden
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }

    fn dump(&self, _: &App) -> PanelState {
        let mut state = PanelState::new(self.panel_name());
        state.info = PanelInfo::panel(serde_json::json!({ "hidden": self.hidden }));
        state
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if !active {
            self.clear_panel_requests(cx);
        }
    }
}

impl Panel for DockPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        div()
            .id(("dock-title", self.kind as usize))
            .min_w_0()
            .truncate()
            .child(self.kind.title())
            .context_menu(move |menu, window, cx| Self::menu(panel.clone(), menu, window, cx))
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }
}

impl Render for DockPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let contents = match self.kind {
            PanelKind::Timer => self.timer(cx),
            PanelKind::Slides => self.slides(cx),
            PanelKind::Current => self.current(cx),
        };
        let panel = cx.entity().downgrade();
        let measured_panel = panel.clone();
        let previous = self.viewport;
        let measure = canvas(
            move |bounds, window, cx| {
                let viewport = DockViewport {
                    width: bounds.size.width.as_f32().floor() as u32,
                    height: bounds.size.height.as_f32().floor() as u32,
                };
                if viewport != previous {
                    let panel = measured_panel.clone();
                    window.defer(cx, move |_, cx| {
                        _ = panel.update(cx, |panel, cx| {
                            if panel.viewport != viewport {
                                panel.viewport = viewport;
                                cx.notify();
                            }
                        });
                    });
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .inset_0();
        div()
            .id(("dock-body", self.kind as usize))
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .child(contents)
            .child(measure)
            .context_menu(move |menu, window, cx| Self::menu(panel.clone(), menu, window, cx))
    }
}

fn panel_message(message: &'static str, cx: &App) -> Div {
    div()
        .size_full()
        .min_h_0()
        .min_w_0()
        .flex()
        .items_center()
        .justify_center()
        .p_1()
        .text_center()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(message)
}

fn slide_list_item(
    model: &Entity<Presentation>,
    geometry: SlideListGeometry,
    item: usize,
    selected_page: usize,
    cx: &App,
) -> Div {
    let pages = geometry.pages_in_item(item);
    let item_extent = if pages.is_empty() {
        geometry.edge_space()
    } else {
        geometry.item_extent() as f32
    };
    let row = div().flex().items_center().justify_center();
    let row = if geometry.horizontal() {
        row.w(px(item_extent))
            .h(px(geometry.viewport.height as f32))
    } else {
        row.w(px(geometry.viewport.width as f32)).h(px(item_extent))
    };
    if pages.is_empty() {
        return row;
    }
    row.child(
        div()
            .flex()
            .flex_none()
            .w(px(geometry.card_width as f32 * geometry.columns as f32))
            .children(pages.map(|page| slide_card(model, page, selected_page, geometry, cx))),
    )
}

fn slide_card(
    model: &Entity<Presentation>,
    page: usize,
    selected_page: usize,
    geometry: SlideListGeometry,
    cx: &App,
) -> AnyElement {
    let selected = page == selected_page;
    let hover = cx.theme().list_hover;
    let model_for_click = model.clone();
    let card = div()
        .id(("slide-card", page))
        .flex_none()
        .w(px(geometry.card_width as f32))
        .h(px(geometry.card_height as f32))
        .when(
            geometry.card_width >= 24 && geometry.card_height >= 24,
            |card| card.p_1(),
        )
        .overflow_hidden()
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            model_for_click.update(cx, |model, cx| model.navigate_to(page, cx));
        });
    let mut content = div()
        .size_full()
        .flex()
        .flex_col()
        .justify_center()
        .items_center()
        .gap_1()
        .border_1()
        .border_color(transparent_black())
        .rounded_md()
        .hover(move |style| style.bg(hover))
        .when(selected, |style| {
            style
                .bg(cx.theme().list_active)
                .border_color(cx.theme().list_active_border)
        });
    if let Some(width) = geometry.thumbnail_width {
        let slide = model.read(cx);
        let preview = div()
            .w(px(width as f32))
            .max_w_full()
            .h(px((width * 3 / 4) as f32))
            .flex_shrink_0()
            .overflow_hidden()
            .rounded_sm()
            .bg(cx.theme().muted);
        content = content.child(match slide.thumbnail(page) {
            Some(image) => preview.child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        if let Some((image_bounds, _)) =
                            fitted_page(bounds, window.scale_factor(), raster_size(&image))
                        {
                            paint_page(bounds, image_bounds, image, window);
                        }
                    },
                )
                .size_full(),
            ),
            None if slide.thumbnail_error(page) => preview.child(panel_message("描画失敗", cx)),
            None => preview.child(panel_message("未描画", cx)),
        });
    }
    let page_number = div()
        .flex_shrink_0()
        .min_w_0()
        .max_w_full()
        .truncate()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(if selected {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        })
        .child(format!("{}", page + 1));
    card.child(content.child(page_number)).into_any_element()
}

fn edit_countdown(model: Entity<Presentation>, window: &mut Window, cx: &mut App) {
    let seconds = model.read(cx).settings().timer.countdown_seconds;
    let input = cx.new(|cx| {
        InputState::new(window, cx).default_value(format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        ))
    });
    let bad_input = cx.new(|_| false);
    let initial_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            input.focus_handle(cx).focus(window, cx);
        }
        let bad = *bad_input.read(cx);
        let input_for_save = input.clone();
        let bad_input = bad_input.clone();
        let model = model.clone();
        dialog
            .title("カウントダウン時間")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&input))
                    .child(
                        div()
                            .text_sm()
                            .text_color(if bad {
                                cx.theme().danger
                            } else {
                                cx.theme().muted_foreground
                            })
                            .child("分:秒 または 時:分:秒（1秒〜99:59:59）"),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label("キャンセル")))
                    .child(
                        DialogAction::new()
                            .child(Button::new("save-countdown").primary().label("保存")),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let value = input_for_save.read(cx).value();
                let Some(seconds) = parse_duration(&value) else {
                    bad_input.update(cx, |bad, cx| {
                        *bad = true;
                        cx.notify();
                    });
                    window.refresh();
                    return false;
                };
                model.update(cx, |model, cx| {
                    let mut settings = model.settings().timer;
                    settings.countdown_seconds = seconds;
                    model.set_timer_settings(settings, cx);
                });
                true
            })
    });
}

fn parse_duration(value: &str) -> Option<u32> {
    let (head, seconds) = value.trim().rsplit_once(':')?;
    let seconds = seconds.parse::<u32>().ok().filter(|&value| value < 60)?;
    let total = if let Some((hours, minutes)) = head.split_once(':') {
        let hours = hours.parse::<u32>().ok().filter(|&value| value < 100)?;
        let minutes = minutes.parse::<u32>().ok().filter(|&value| value < 60)?;
        hours * 3600 + minutes * 60 + seconds
    } else {
        head.parse::<u32>()
            .ok()?
            .checked_mul(60)?
            .checked_add(seconds)?
    };
    (1..=MAX_COUNTDOWN_SECONDS)
        .contains(&total)
        .then_some(total)
}

#[cfg(test)]
mod tests {
    use super::{
        DockAreaState, DockViewport, LAYOUT_VERSION, MAX_COUNTDOWN_SECONDS, PanelInfo, PanelKind,
        PanelState, Pixels, SlideList, SlideListGeometry, SlideListLayout, SlideListMode,
        SlideListSettings, ThumbnailSize, parse_duration, px, valid_layout,
    };

    fn slide_settings(layout: SlideListLayout) -> SlideListSettings {
        SlideListSettings {
            mode: SlideListMode::Thumbnails,
            size: ThumbnailSize::Large,
            layout,
        }
    }

    #[test]
    fn countdown_input_covers_minutes_hours_and_limits() {
        assert_eq!(parse_duration("10:00"), Some(600));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("99:59:59"), Some(MAX_COUNTDOWN_SECONDS));
        for invalid in ["0:00", "0:60", "1:60:00", "100:00:00", "4294967295:00", "1"] {
            assert_eq!(parse_duration(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn only_singleton_dock_layouts_restore() {
        let mut state = DockAreaState {
            version: Some(LAYOUT_VERSION),
            center: PanelState {
                panel_name: "TabPanel".into(),
                children: PanelKind::ALL
                    .map(|kind| PanelState::new(kind.name()))
                    .to_vec(),
                info: PanelInfo::tabs(0),
            },
            ..Default::default()
        };
        assert!(valid_layout(&state));
        state.center.children[1] = state.center.children[0].clone();
        assert!(!valid_layout(&state));
    }

    #[test]
    fn narrow_docks_reduce_thumbnail_width_and_keep_controls_stacked() {
        let narrow = DockViewport {
            width: 96,
            height: 400,
        };
        let settings = slide_settings(SlideListLayout::Grid);
        let geometry = SlideListGeometry::new(narrow, settings, 8);
        assert!(!narrow.inline_controls());
        assert_eq!(geometry.thumbnail_width, Some(80));
        assert_eq!(geometry.columns, 1);
        let squeezed = SlideListGeometry::new(
            DockViewport {
                width: 50,
                ..narrow
            },
            settings,
            8,
        );
        assert_eq!(squeezed.thumbnail_width, None);
        assert_eq!(squeezed.card_width, 50);
    }

    #[test]
    fn short_docks_fit_thumbnails_and_controls_side_by_side() {
        let short = DockViewport {
            width: 400,
            height: 96,
        };
        let settings = slide_settings(SlideListLayout::Horizontal);
        assert!(short.inline_controls());
        let geometry = SlideListGeometry::new(short, settings, 8);
        assert!(geometry.horizontal());
        assert_eq!(geometry.thumbnail_width, Some(80));
        assert_eq!(geometry.card_height, short.height);
        let page_numbers = SlideListGeometry::new(
            short,
            SlideListSettings {
                mode: SlideListMode::PageNumbers,
                ..settings
            },
            8,
        );
        assert_eq!(page_numbers.thumbnail_width, None);
        assert_eq!(page_numbers.visible_pages(px(0.), px(400.)), 0..0);
    }

    #[test]
    fn slide_lists_center_the_first_last_and_only_page_without_scrolling_past_edges() {
        let viewport = DockViewport {
            width: 701,
            height: 501,
        };
        for direction in [
            SlideListLayout::Vertical,
            SlideListLayout::Horizontal,
            SlideListLayout::Grid,
        ] {
            for count in [1, 10] {
                let geometry = SlideListGeometry::new(viewport, slide_settings(direction), count);
                let sizes: Vec<_> = geometry
                    .item_sizes()
                    .iter()
                    .map(|size| {
                        if geometry.horizontal() {
                            size.width
                        } else {
                            size.height
                        }
                    })
                    .collect();
                let view = px(geometry.viewport_extent() as f32);
                let content = sizes.iter().copied().sum::<Pixels>();
                for page in [0, count - 1] {
                    let item = geometry.item_for_page(page);
                    let item_center =
                        sizes[..item].iter().copied().sum::<Pixels>() + sizes[item] / 2.0;
                    let desired = item_center - view / 2.0;
                    assert!(desired >= px(0.) && desired <= content - view);
                    assert_eq!(item_center - desired, view / 2.0);
                }
            }
        }
    }

    #[test]
    fn grid_maps_incomplete_last_row_and_excludes_spacers_from_preload() {
        let viewport = DockViewport {
            width: 650,
            height: 501,
        };
        let geometry = SlideListGeometry::new(viewport, slide_settings(SlideListLayout::Grid), 7);
        assert_eq!(geometry.columns, 3);
        assert_eq!(geometry.rows(), 3);
        assert_eq!(geometry.pages_in_item(0), 0..0);
        assert_eq!(geometry.pages_in_item(1), 0..3);
        assert_eq!(geometry.pages_in_item(geometry.item_for_page(6)), 6..7);
        assert_eq!(geometry.pages_in_item(4), 0..0);

        let end = px(-2.0 * geometry.item_extent() as f32);
        assert_eq!(
            geometry.visible_pages(end, px(viewport.height as f32)),
            3..7
        );
    }

    #[test]
    fn recenter_only_changes_with_the_page_document_or_layout() {
        let viewport = DockViewport {
            width: 450,
            height: 501,
        };
        let settings = slide_settings(SlideListLayout::Vertical);
        let geometry = SlideListGeometry::new(viewport, settings, 7);
        let mut list = SlideList::new(geometry);
        assert_eq!(list.center_on(3, 1), Some(2));
        assert_eq!(list.center_on(3, 1), None);
        assert_eq!(list.center_on(3, 3), Some(4));
        assert_eq!(list.center_on(4, 3), Some(4));
        assert_eq!(list.center_on(4, 3), None);

        let mut resized = SlideList::new(SlideListGeometry::new(
            DockViewport {
                height: 450,
                ..viewport
            },
            settings,
            7,
        ));
        assert_eq!(resized.center_on(4, 3), Some(4));
    }
}
