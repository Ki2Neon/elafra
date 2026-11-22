use std::{
    cell::Cell,
    path::{Path, PathBuf},
};

use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    dialog::{DialogAction, DialogClose, DialogFooter},
    dock::DockEvent,
    input::{Input, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    radio::RadioGroup,
    theme::Theme,
    tooltip::Tooltip,
};
use gpui_kit::{
    App, AppContext, Context, DisplayId, Entity, ExternalPaths, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Role, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, div, prelude::FluentBuilder, px,
};

use crate::{
    actions::{
        EndPresentation, NextSlide, OpenPdf, PreviousSlide, StartPresentation, ToggleFullscreen,
    },
    audience,
    docks::{PanelKind, WorkspaceDocks},
    presentation::{PasswordPrompt, Presentation},
    settings::{MAX_AUDIENCE_CACHE_MIB, ThemePreference},
};

pub(crate) struct Presenter {
    model: Entity<Presentation>,
    docks: WorkspaceDocks,
    focus: FocusHandle,
    output: Option<DisplayId>,
    active_output: Option<DisplayId>,
    picker: Option<Task<()>>,
    password_dialog: Option<u64>,
    _model_subscription: Subscription,
    _dock_subscription: Subscription,
}

impl Presenter {
    pub(crate) fn new(
        model: Entity<Presentation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let saved_layout = model.read(cx).settings().dock.clone();
        let docks = WorkspaceDocks::new(model.clone(), saved_layout, window, cx);
        let focus = cx.focus_handle();
        focus.focus(window, cx);

        let model_subscription = cx.observe_in(&model, window, |this, _, window, cx| {
            this.update_password_dialog(window, cx);
            cx.notify();
        });
        let dock_subscription = cx.subscribe(&docks.area, |this, _, event: &DockEvent, cx| {
            if matches!(event, DockEvent::LayoutChanged) {
                let layout = this.docks.layout(cx);
                this.model
                    .update(cx, |model, cx| model.set_dock_layout(layout, cx));
            }
        });

        Self {
            model,
            docks,
            focus,
            output: None,
            active_output: None,
            picker: None,
            password_dialog: None,
            _model_subscription: model_subscription,
            _dock_subscription: dock_subscription,
        }
    }

    fn pick_pdf(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() || window.has_active_dialog(cx) {
            return;
        }
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("PDFを読み込む")
            .add_filter("PDF", &["pdf"])
            .set_parent(window);
        let this = cx.weak_entity();
        self.picker = Some(window.spawn(cx, async move |cx| {
            let file = dialog.pick_file().await;
            let _ = cx.update(|_, cx| {
                let _ = this.update(cx, |this, cx| {
                    if let Some(file) = file {
                        Self::open_path(&this.model, file.path().to_owned(), cx);
                    }
                    let completion = this.picker.take();
                    if let Some(completion) = completion {
                        completion.detach();
                    }
                });
            });
        }));
    }

    fn open_path(model: &Entity<Presentation>, path: PathBuf, cx: &mut App) {
        let size = audience::largest_raster_size(cx);
        model.update(cx, |model, cx| {
            model.set_raster_size(size, cx);
            model.open_file(path, cx);
        });
    }

    fn update_password_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.model.read(cx).password_prompt().cloned();
        if let Some(prompt) = prompt {
            if self.password_dialog != Some(prompt.id) {
                self.password_dialog = Some(prompt.id);
                self.open_password_dialog(prompt, window, cx);
            }
        } else {
            self.password_dialog = None;
        }
    }

    fn open_password_dialog(
        &self,
        prompt: PasswordPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("パスワード")
                .masked(true)
        });
        let initial_focus = Cell::new(true);
        let model = self.model.clone();
        let file = file_label(&prompt.path);
        window.open_dialog(cx, move |dialog, window, cx| {
            if initial_focus.replace(false) {
                input.focus_handle(cx).focus(window, cx);
            }
            let submit_model = model.clone();
            let cancel_model = model.clone();
            let submit_input = input.clone();
            dialog
                .title("PDFのパスワード")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .truncate()
                                .text_color(cx.theme().muted_foreground)
                                .child(file.clone()),
                        )
                        .when(prompt.wrong_password, |content| {
                            content.child(
                                div()
                                    .text_color(cx.theme().danger)
                                    .child("パスワードが違う。もう一度入力。"),
                            )
                        })
                        .child(Input::new(&input)),
                )
                .on_ok(move |_, _, cx| {
                    let password = submit_input.read(cx).value().to_string();
                    submit_model.update(cx, |model, cx| {
                        model.submit_password(prompt.id, password, cx)
                    });
                    true
                })
                .on_close(move |_, _, cx| {
                    cancel_model.update(cx, |model, cx| model.cancel_password(prompt.id, cx));
                })
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("キャンセル")))
                        .child(
                            DialogAction::new()
                                .child(Button::new("open-encrypted").primary().label("開く")),
                        ),
                )
        });
    }

    fn open_audience(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) || self.model.read(cx).page_count() == 0 {
            return;
        }
        let size = audience::largest_raster_size(cx);
        self.model
            .update(cx, |model, cx| model.set_raster_size(size, cx));
        let target = audience::resolve(self.output, window, cx);
        if self.model.read(cx).is_presenting() && self.active_output == target.id() {
            return;
        }
        let target_id = target.id();
        let warning_model = self.model.clone();
        match audience::open(
            self.model.clone(),
            target,
            move |cx| {
                warning_model.update(cx, |model, cx| {
                    model.set_notice(
                        "指定先へ投影できなかった。スライド画面を移動し、F11 で表示。".into(),
                        cx,
                    );
                });
            },
            cx,
        ) {
            Ok(handle) => {
                self.active_output = target_id;
                self.model
                    .update(cx, |model, cx| model.show_audience(handle, cx));
            }
            Err(error) => {
                self.model.update(cx, |model, cx| {
                    model.set_notice(format!("スライド画面を開けなかった。{error}"), cx);
                });
            }
        }
    }

    fn select_output(
        &mut self,
        output: Option<DisplayId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.output = output;
        if self.model.read(cx).is_presenting() {
            self.open_audience(window, cx);
        }
        cx.notify();
    }

    fn navigate(&mut self, delta: i32, window: &mut Window, cx: &mut Context<Self>) {
        if !window.has_active_dialog(cx) {
            self.model
                .update(cx, |model, cx| model.navigate_relative(delta, cx));
        }
    }

    fn end_audience(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.has_active_dialog(cx) {
            self.model
                .update(cx, |model, cx| model.end_presentation(cx));
        }
    }

    fn open_settings(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.is_some() || window.has_active_dialog(cx) {
            return;
        }
        let settings = self.model.read(cx).settings();
        let theme = settings.theme;
        let capacity = settings.audience_cache_mib;
        let cache_input =
            cx.new(|cx| InputState::new(window, cx).default_value(capacity.to_string()));
        let draft = cx.new(|_| PreferencesDraft {
            theme,
            cache_input,
            invalid_capacity: false,
        });
        let model = self.model.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let save_draft = draft.clone();
            let save_model = model.clone();
            dialog
                .title("設定")
                .child(draft.clone())
                .on_ok(move |_, window, cx| {
                    let draft = save_draft.read(cx);
                    let theme = draft.theme;
                    let input = draft.cache_input.read(cx).value();
                    let Some(capacity) = parse_cache_mib(input.as_ref()) else {
                        save_draft.update(cx, |draft, cx| {
                            draft.invalid_capacity = true;
                            cx.notify();
                        });
                        return false;
                    };
                    let theme_changed = theme != save_model.read(cx).settings().theme;
                    save_model.update(cx, |model, cx| model.set_preferences(theme, capacity, cx));
                    if theme_changed {
                        Theme::change(theme.gpui_mode(), Some(window), cx);
                    }
                    true
                })
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(|button| button.label("キャンセル")))
                        .child(
                            DialogAction::new()
                                .child(Button::new("save-settings").primary().label("保存")),
                        ),
                )
        });
    }

    fn file_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let presenter = cx.weak_entity();
        let model = self.model.clone();
        Button::new("file-menu")
            .ghost()
            .small()
            .label("ファイル")
            .dropdown_menu(move |menu, window, cx| {
                let presenter = presenter.clone();
                let recent = model.read(cx).settings().recent.clone();
                let menu = menu.item(
                    PopupMenuItem::new("PDFを読み込む…")
                        .icon(IconName::FolderOpen)
                        .on_click(move |_, window, cx| {
                            let _ = presenter.update(cx, |this, cx| this.pick_pdf(window, cx));
                        }),
                );
                let model = model.clone();
                menu.separator()
                    .submenu("最近開いたPDF", window, cx, move |menu, _, _| {
                        if recent.is_empty() {
                            return menu.label("履歴なし");
                        }
                        recent.iter().fold(menu, |menu, path| {
                            let path = path.clone();
                            let model = model.clone();
                            menu.item(PopupMenuItem::new(file_label(&path)).on_click(
                                move |_, _, cx| {
                                    Self::open_path(&model, path.clone(), cx);
                                },
                            ))
                        })
                    })
            })
    }

    fn view_menu(&self, cx: &Context<Self>) -> impl IntoElement {
        let presenter = cx.weak_entity();
        let selected_output = self.output;
        Button::new("view-menu")
            .ghost()
            .small()
            .label("表示")
            .dropdown_menu(move |menu, window, cx| {
                let presenter_for_output = presenter.clone();
                let monitors = audience::displays(window, cx);
                let menu = menu
                    .submenu("投影先", window, cx, move |menu, _, _| {
                        let presenter = presenter_for_output.clone();
                        let menu = menu.item(
                            PopupMenuItem::new("自動 — 別モニターを優先")
                                .checked(selected_output.is_none())
                                .on_click(move |_, window, cx| {
                                    let _ = presenter.update(cx, |this, cx| {
                                        this.select_output(None, window, cx)
                                    });
                                }),
                        );
                        monitors.iter().fold(menu, |menu, monitor| {
                            let presenter = presenter_for_output.clone();
                            let id = monitor.id;
                            let label = if monitor.is_presenter {
                                format!("{} — プレゼンター", monitor.label)
                            } else {
                                monitor.label.clone()
                            };
                            menu.item(
                                PopupMenuItem::new(label)
                                    .checked(selected_output == Some(id))
                                    .on_click(move |_, window, cx| {
                                        let _ = presenter.update(cx, |this, cx| {
                                            this.select_output(Some(id), window, cx)
                                        });
                                    }),
                            )
                        })
                    })
                    .separator();
                PanelKind::ALL.into_iter().fold(menu, |menu, panel| {
                    let presenter = presenter.clone();
                    menu.item(
                        PopupMenuItem::new(panel.title()).on_click(move |_, window, cx| {
                            let _ = presenter.update(cx, |this, cx| {
                                this.docks.show(panel, window, cx);
                                let layout = this.docks.layout(cx);
                                this.model
                                    .update(cx, |model, cx| model.set_dock_layout(layout, cx));
                                cx.notify();
                            });
                        }),
                    )
                })
            })
    }
}

impl Render for Presenter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let count = model.page_count();
        let index = model.presenter_page_index();
        let is_presenting = model.is_presenting();
        let notice = model.notice().map(str::to_owned);
        let memory_notice = model.memory_notice().map(str::to_owned);
        let document = if model.loading() {
            "PDFを読み込み中…".into()
        } else {
            model
                .current_path()
                .map(file_label)
                .unwrap_or_else(|| "PDFを選択".into())
        };
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let border = theme.border;
        let muted = theme.muted_foreground;
        let danger = theme.danger;

        div()
            .id("presenter")
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .key_context("Presenter")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &OpenPdf, window, cx| this.pick_pdf(window, cx)))
            .on_action(cx.listener(|this, _: &NextSlide, window, cx| this.navigate(1, window, cx)))
            .on_action(
                cx.listener(|this, _: &PreviousSlide, window, cx| this.navigate(-1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &StartPresentation, window, cx| {
                    this.open_audience(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &EndPresentation, window, cx| this.end_audience(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleFullscreen, window, cx| {
                if !window.has_active_dialog(cx)
                    && let Some(handle) = this.model.read(cx).audience()
                {
                    audience::toggle_fullscreen(handle, cx);
                }
            }))
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.bg(background.blend(foreground.opacity(0.06)))
            })
            .on_drop(cx.listener(|this, files: &ExternalPaths, window, cx| {
                if window.has_active_dialog(cx) {
                    return;
                }
                if let [path] = files.0.as_slice()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
                {
                    Self::open_path(&this.model, path.clone(), cx);
                } else {
                    this.model.update(cx, |model, cx| {
                        model.set_notice("PDFファイルを1件ドロップ。".into(), cx);
                    });
                }
            }))
            .child(
                div()
                    .h(px(54.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .border_b_1()
                    .border_color(border)
                    .child(self.file_menu(cx))
                    .child(self.view_menu(cx))
                    .child(
                        Button::new("settings")
                            .ghost()
                            .small()
                            .label("設定")
                            .child(Icon::new(IconName::WindowRestore).xsmall())
                            .tooltip("設定ダイアログを開く")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    )
                    .child(div().h(px(20.)).w(px(1.)).flex_none().bg(border))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .overflow_hidden()
                            .text_sm()
                            .text_color(muted)
                            .child(div().min_w_0().truncate().child(document))
                            .when_some(memory_notice, |status, detail| {
                                status.child(
                                    div()
                                        .id("memory-status")
                                        .flex_none()
                                        .role(Role::Status)
                                        .aria_label("メモリ不足")
                                        .aria_description(detail.clone())
                                        .tooltip(move |window, cx| {
                                            Tooltip::new(detail.clone()).build(window, cx)
                                        })
                                        .text_color(danger)
                                        .child("メモリ不足"),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("previous-slide")
                                    .ghost()
                                    .small()
                                    .icon(IconName::ChevronLeft)
                                    .disabled(index == 0)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.navigate(-1, window, cx)
                                    })),
                            )
                            .child(
                                div()
                                    .min_w(px(48.))
                                    .text_center()
                                    .text_sm()
                                    .text_color(muted)
                                    .child(if count > 0 {
                                        format!("{} / {count}", index + 1)
                                    } else {
                                        "—".into()
                                    }),
                            )
                            .child(
                                Button::new("next-slide")
                                    .ghost()
                                    .small()
                                    .icon(IconName::ChevronRight)
                                    .disabled(index + 1 >= count)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.navigate(1, window, cx)
                                    })),
                            ),
                    )
                    .child(
                        Button::new("presentation")
                            .small()
                            .when(is_presenting, |button| {
                                button
                                    .outline()
                                    .icon(IconName::Square)
                                    .label("プレゼン終了")
                            })
                            .when(!is_presenting, |button| {
                                button.primary().icon(IconName::Play).label("プレゼン開始")
                            })
                            .disabled(count == 0)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.model.read(cx).is_presenting() {
                                    this.end_audience(window, cx);
                                } else {
                                    this.open_audience(window, cx);
                                }
                            })),
                    ),
            )
            .when_some(notice, |root, notice| {
                root.child(
                    div()
                        .flex_none()
                        .flex()
                        .justify_between()
                        .items_center()
                        .gap_4()
                        .py_1()
                        .px_4()
                        .bg(danger.opacity(0.10))
                        .text_sm()
                        .text_color(foreground)
                        .child(notice)
                        .child(
                            Button::new("dismiss-notice")
                                .ghost()
                                .small()
                                .icon(IconName::Close)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.model.update(cx, |model, cx| model.clear_notice(cx));
                                })),
                        ),
                )
            })
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(self.docks.area.clone()),
            )
    }
}

struct PreferencesDraft {
    theme: ThemePreference,
    cache_input: Entity<InputState>,
    invalid_capacity: bool,
}

impl Render for PreferencesDraft {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let danger = cx.theme().danger;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().child("テーマ"))
                    .child(
                        RadioGroup::horizontal("theme")
                            .children(["ライト", "ダーク"])
                            .selected_index(Some(usize::from(self.theme == ThemePreference::Dark)))
                            .on_change(cx.listener(|draft, index: &usize, _, cx| {
                                draft.theme = if *index == 0 {
                                    ThemePreference::Light
                                } else {
                                    ThemePreference::Dark
                                };
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().child("観客用キャッシュ容量"))
                    .child(
                        Input::new(&self.cache_input)
                            .aria_label("観客用キャッシュ容量 MB")
                            .suffix(div().text_sm().text_color(muted).child("MB")),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child("0 MBで観客用の先読みを無効にする。"),
                    )
                    .when(self.invalid_capacity, |section| {
                        section.child(div().text_sm().text_color(danger).child(format!(
                            "0〜{MAX_AUDIENCE_CACHE_MIB} MBの範囲で整数を入力。"
                        )))
                    }),
            )
    }
}

fn parse_cache_mib(input: &str) -> Option<u64> {
    input
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|capacity| *capacity <= MAX_AUDIENCE_CACHE_MIB)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::{MAX_AUDIENCE_CACHE_MIB, parse_cache_mib};

    #[test]
    fn cache_capacity_accepts_zero_and_rejects_invalid_numbers() {
        assert_eq!(parse_cache_mib("0"), Some(0));
        assert_eq!(parse_cache_mib(" 128 "), Some(128));
        assert_eq!(
            parse_cache_mib(&MAX_AUDIENCE_CACHE_MIB.to_string()),
            Some(MAX_AUDIENCE_CACHE_MIB)
        );
        for invalid in ["", "-1", "1.5", "128 MB", "18446744073709551615"] {
            assert_eq!(parse_cache_mib(invalid), None);
        }
    }
}
