use super::*;

use anyhow::Context as _;
use base64::Engine as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerSubmitAction {
    Send,
    Preparing,
    Stop,
}

pub(super) fn composer_submit_action(
    status: Option<SessionStatus>,
    preparing: bool,
) -> ComposerSubmitAction {
    if preparing {
        ComposerSubmitAction::Preparing
    } else if status.is_some_and(SessionStatus::is_busy) {
        ComposerSubmitAction::Stop
    } else {
        ComposerSubmitAction::Send
    }
}

impl Waku {
    // ── Permission ─────────────────────────────────────────────────────────

    pub(super) fn render_permission(&self, cx: &mut Context<Self>) -> Option<Div> {
        if let Some(input) = self.selected_runtime()?.pending_user_input.clone() {
            return Some(self.render_user_input(input, cx));
        }
        let permission = self.selected_runtime()?.pending_permission.as_ref()?;
        let theme = Theme::current(cx);
        let request_id = permission.request_id.clone();
        let mut buttons = div().flex().items_center().gap(px(8.0)).mt(px(10.0));
        for option in &permission.options {
            let request_id = request_id.clone();
            let option_id = option.id.clone();
            let allow = option.allow;
            buttons = buttons.child(
                div()
                    .id(SharedString::from(format!(
                        "permission-{}-{}",
                        permission.request_id, option.id
                    )))
                    .h(px(28.0))
                    .px(px(13.0))
                    .rounded(px(7.0))
                    .flex()
                    .items_center()
                    .cursor_default()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(allow, |element| {
                        element
                            .bg(theme.inverse)
                            .text_color(theme.on_inverse)
                            .hover(|element| element.opacity(0.9))
                    })
                    .when(!allow, |element| {
                        element
                            .border_1()
                            .border_color(theme.border_strong)
                            .text_color(theme.text_secondary)
                            .hover(|element| element.bg(theme.overlay).text_color(theme.text))
                    })
                    .active(|element| element.opacity(0.8))
                    .child(SharedString::from(option.label.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.respond_permission(request_id.clone(), option_id.clone(), cx);
                    })),
            );
        }
        Some(
            div().px(px(20.0)).pb(px(8.0)).child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.raised)
                    .shadow_md()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(icon("icons/alert.svg", 13.0, theme.warning))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(permission.title.clone())),
                            ),
                    )
                    .child(
                        div()
                            .id("permission-detail")
                            .mt(px(8.0))
                            .max_h(px(92.0))
                            .overflow_y_scroll()
                            .p(px(8.0))
                            .rounded(px(7.0))
                            .bg(theme.inset)
                            .font_family(crate::md::render::MONO_FAMILY)
                            .text_size(px(10.5))
                            .line_height(px(16.0))
                            .text_color(theme.text_secondary)
                            .whitespace_normal()
                            .child(SharedString::from(permission.detail.clone())),
                    )
                    .child(buttons),
            ),
        )
    }

    fn render_user_input(&self, pending: PendingUserInput, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let Some(question) = pending.current_question().cloned() else {
            return div();
        };
        let selected = pending
            .selections
            .get(&question.id)
            .cloned()
            .unwrap_or_default();
        let has_custom = pending
            .custom_answers
            .get(&question.id)
            .is_some_and(|answer| !answer.trim().is_empty());
        let can_continue = has_custom || !selected.is_empty();
        let is_last = pending.question_index + 1 == pending.questions.len();
        let request_id = pending.request_id.clone();
        let question_index = pending.question_index;
        let mut options = div().mt(px(9.0)).flex().flex_col().gap(px(4.0));
        for (index, option) in question.options.iter().enumerate() {
            let is_selected = selected.iter().any(|answer| answer == &option.label);
            let click_label = option.label.clone();
            let key_label = option.label.clone();
            let focus = self.transcript_control_focus(
                format!("user-input-{request_id}-{question_index}-option-{index}"),
                cx,
            );
            options = options.child(
                div()
                    .id(SharedString::from(format!(
                        "user-input-{request_id}-{question_index}-option-{index}"
                    )))
                    .track_focus(&focus)
                    .tab_index(0)
                    .tab_stop(true)
                    .min_h(px(36.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(if is_selected {
                        theme.accent.opacity(0.34)
                    } else {
                        theme.border.opacity(0.0)
                    })
                    .bg(if is_selected {
                        theme.accent.opacity(0.08)
                    } else {
                        theme.overlay
                    })
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_default()
                    .focus_visible(|style| style.border_color(theme.accent))
                    .when(!is_selected, |row| {
                        row.hover(|style| style.border_color(theme.border).bg(theme.overlay_strong))
                    })
                    .active(|style| style.opacity(0.85))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(option.label.clone())),
                            )
                            .children(option.description.as_ref().map(|description| {
                                div()
                                    .mt(px(1.0))
                                    .text_size(px(10.0))
                                    .line_height(px(13.0))
                                    .text_color(theme.text_secondary)
                                    .whitespace_normal()
                                    .child(SharedString::from(description.clone()))
                            })),
                    )
                    .when(is_selected, |row| {
                        row.child(icon("icons/check.svg", 12.0, theme.accent))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_user_input_option(click_label.clone(), cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.select_user_input_option(key_label.clone(), cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }

        let next_focus = self.transcript_control_focus(
            format!("user-input-{request_id}-{question_index}-continue"),
            cx,
        );
        let back = (question_index > 0).then(|| {
            let focus = self.transcript_control_focus(
                format!("user-input-{request_id}-{question_index}-back"),
                cx,
            );
            div()
                .id(SharedString::from(format!(
                    "user-input-{request_id}-{question_index}-back"
                )))
                .track_focus(&focus)
                .tab_index(0)
                .tab_stop(true)
                .h(px(26.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .cursor_default()
                .text_size(px(10.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_tertiary)
                .focus_visible(|style| style.border_1().border_color(theme.accent))
                .hover(|style| style.bg(theme.overlay).text_color(theme.text_secondary))
                .active(|style| style.opacity(0.8))
                .child(tr!("user_input.back"))
                .on_click(cx.listener(|this, _, _, cx| this.previous_user_input(cx)))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        this.previous_user_input(cx);
                        cx.stop_propagation();
                    }
                }))
        });
        let continue_button = div()
            .id(SharedString::from(format!(
                "user-input-{request_id}-{question_index}-continue"
            )))
            .track_focus(&next_focus)
            .tab_index(0)
            .tab_stop(can_continue)
            .h(px(26.0))
            .px(px(10.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .cursor_default()
            .text_size(px(10.5))
            .font_weight(FontWeight::SEMIBOLD)
            .bg(if can_continue {
                theme.inverse
            } else {
                theme.overlay
            })
            .text_color(if can_continue {
                theme.on_inverse
            } else {
                theme.text_ghost
            })
            .when(can_continue, |button| {
                button
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .hover(|style| style.opacity(0.9))
                    .active(|style| style.opacity(0.8))
                    .on_click(cx.listener(|this, _, _, cx| this.advance_user_input(cx)))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.advance_user_input(cx);
                            cx.stop_propagation();
                        }
                    }))
            })
            .child(if is_last {
                tr!("user_input.submit")
            } else {
                tr!("user_input.next")
            });

        let progress = (pending.questions.len() > 1).then(|| {
            div()
                .h(px(18.0))
                .px(px(6.0))
                .rounded(px(5.0))
                .bg(theme.overlay)
                .flex()
                .items_center()
                .text_size(px(9.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_tertiary)
                .child(tr!(
                    "user_input.progress",
                    current = question_index + 1,
                    total = pending.questions.len()
                ))
        });

        div().flex_none().px(px(20.0)).pb(px(8.0)).child(
            div()
                .id(SharedString::from(format!("user-input-{request_id}")))
                .w_full()
                .max_w(px(CONTENT_MAX_WIDTH))
                .mx_auto()
                .px(px(14.0))
                .pt(px(12.0))
                .pb(px(10.0))
                .rounded(px(13.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.composer)
                .tab_index(0)
                .tab_group()
                .tab_stop(false)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from(question.header.clone())),
                        )
                        .children(progress),
                )
                .child(
                    div()
                        .mt(px(5.0))
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .whitespace_normal()
                        .child(SharedString::from(question.question.clone())),
                )
                .children((!question.options.is_empty()).then_some(options))
                .child(
                    div()
                        .mt(px(if question.options.is_empty() {
                            9.0
                        } else {
                            4.0
                        }))
                        .h(px(34.0))
                        .px(px(10.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(if has_custom {
                            theme.accent.opacity(0.34)
                        } else {
                            theme.border.opacity(0.0)
                        })
                        .bg(if has_custom {
                            theme.accent.opacity(0.06)
                        } else {
                            theme.overlay
                        })
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .text_size(px(11.5))
                        .line_height(px(16.0))
                        .child(icon(
                            "icons/pencil.svg",
                            11.0,
                            if has_custom {
                                theme.accent
                            } else {
                                theme.text_ghost
                            },
                        ))
                        .child(self.user_input_answer.clone()),
                )
                .child(
                    div()
                        .mt(px(8.0))
                        .flex()
                        .items_center()
                        .children(back)
                        .child(div().flex_1())
                        .child(continue_button),
                ),
        )
    }

    // ── Composer ───────────────────────────────────────────────────────────

    pub(super) fn render_provider_model_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let session = self.selected_session();
        let provider = session
            .map(|session| session.provider.clone())
            .or_else(|| {
                self.state
                    .external_providers
                    .first()
                    .map(|provider| provider.id.clone())
            })
            .unwrap_or_else(|| ProviderId::new(""));
        let selected_model = session.and_then(|session| self.model_for_session(session));
        let selected_model_name = self.model_display_name(&provider, selected_model);
        let has_providers = !self.state.external_providers.is_empty();
        if !has_providers {
            return div()
                .h(px(24.0))
                .px(px(7.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_color(theme.text_tertiary)
                .child(tr!("providers.configure_first"))
                .into_any_element();
        }

        let favorites = self.state.favorite_models.clone();
        let query = self
            .model_search
            .read(cx)
            .content()
            .trim()
            .to_ascii_lowercase();
        let searching = !query.is_empty();
        let selected_tab = self.model_picker_tab.clone();
        let locked_provider = session
            .filter(|session| !session.messages.is_empty())
            .map(|session| session.provider.clone());
        let available_models = Rc::new(visible_picker_models(
            &self.state.external_providers,
            &self.model_catalogs,
            &favorites,
            locked_provider.as_ref(),
            &selected_tab,
            &query,
        ));
        let providers = self.state.external_providers.clone();
        let catalogs = self.model_catalogs.clone();
        let weak = cx.entity().downgrade();
        let handle = self.menu_handle_with(MODEL_PICKER_MENU_ID, cx, move |open, window, cx| {
            if open {
                let _ = weak.update(cx, |this, cx| {
                    this.model_picker_highlight = None;
                    cx.notify();
                });
            } else {
                let focus = weak
                    .read_with(cx, |this, cx| this.composer.read(cx).focus())
                    .ok();
                if let Some(focus) = focus {
                    window.focus(&focus, cx);
                }
            }
        });
        let weak = cx.entity().downgrade();
        let rows_models = available_models.clone();
        let rows_favorites = favorites.clone();
        let selected_provider = provider.clone();
        let selected_model = selected_model.map(str::to_owned);
        let endpoint_names = providers
            .iter()
            .map(|provider| (provider.id.clone(), provider.name.clone()))
            .collect::<HashMap<_, _>>();
        let popup = move |popover: &ContextMenuHandle, _window: &mut Window, _cx: &mut App| {
            let mut rows = div()
                .id("endpoint-model-list")
                .w(px(360.0))
                .p(px(8.0))
                .flex()
                .flex_col()
                .gap(px(3.0));
            if rows_models.is_empty() {
                rows = rows.child(div().p(px(12.0)).text_color(theme.text_tertiary).child(
                    if searching {
                        tr!("models.none_found")
                    } else {
                        tr!("models.favorite_hint")
                    },
                ));
            }
            for (index, (endpoint, model)) in rows_models.iter().enumerate() {
                let endpoint_name = endpoint_names
                    .get(endpoint)
                    .cloned()
                    .unwrap_or_else(|| endpoint.to_string());
                let label = format!("{endpoint_name} · {}", model.id);
                let selected = *endpoint == selected_provider
                    && selected_model.as_deref() == Some(model.id.as_str());
                let favorite = rows_favorites
                    .iter()
                    .any(|entry| entry.provider == *endpoint && entry.model == model.id);
                let catalog_entry = catalogs
                    .get(endpoint)
                    .and_then(|catalog| catalog.models.iter().find(|entry| entry.id == model.id));
                let supported = catalog_entry_selectable(catalog_entry);
                let unsupported_reason = catalog_entry
                    .and_then(|entry| entry.unsupported_reason.as_ref())
                    .map(|reason| format!("{reason:?}"));
                let choose = weak.clone();
                let choose_key = weak.clone();
                let close = popover.clone();
                let close_key = popover.clone();
                let endpoint_id = endpoint.clone();
                let endpoint_key = endpoint.clone();
                let model_id = model.id.clone();
                let model_key = model.id.clone();
                rows = rows.child(
                    div()
                        .id(SharedString::from(format!("endpoint-model-{index}")))
                        .tab_index(0)
                        .focus_visible(|style| style.border_1().border_color(theme.accent))
                        .px(px(10.0))
                        .py(px(8.0))
                        .rounded(px(7.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .when(selected, |element| element.bg(theme.overlay_strong))
                        .hover(|element| element.bg(theme.overlay))
                        .child(
                            div()
                                .flex_1()
                                .text_color(if supported {
                                    theme.text
                                } else {
                                    theme.text_tertiary
                                })
                                .child(if let Some(reason) = unsupported_reason {
                                    format!("{label} ({reason})")
                                } else {
                                    label
                                }),
                        )
                        .child(icon(
                            if favorite {
                                "icons/star-filled.svg"
                            } else {
                                "icons/star.svg"
                            },
                            13.0,
                            theme.text_tertiary,
                        ))
                        .on_click(move |_, window, cx| {
                            if !supported {
                                return;
                            }
                            let _ = choose.update(cx, |this, cx| {
                                this.choose_model(endpoint_id.clone(), model_id.clone(), cx)
                            });
                            close.close(window, cx);
                        })
                        .on_key_down({
                            move |event: &KeyDownEvent, window, cx| {
                                if !supported {
                                    return;
                                }
                                if !event.keystroke.modifiers.modified()
                                    && matches!(event.keystroke.key.as_str(), "enter" | "space")
                                {
                                    let _ = choose_key.update(cx, |this, cx| {
                                        this.choose_model(
                                            endpoint_key.clone(),
                                            model_key.clone(),
                                            cx,
                                        )
                                    });
                                    close_key.close(window, cx);
                                    cx.stop_propagation();
                                }
                            }
                        }),
                );
            }
            div()
                .rounded(px(10.0))
                .border_1()
                .border_color(theme.border_strong)
                .bg(theme.raised)
                .shadow_lg()
                .child(rows)
                .into_any_element()
        };
        popover(
            MenuChip::new("composer-provider-model")
                .icon(
                    provider_icon(&provider),
                    provider_color(&theme, &provider).opacity(0.9),
                )
                .label(selected_model_name)
                .caret(false)
                .selected(handle.is_open()),
            &handle,
            MenuAlign::AboveLeft,
            popup,
        )
        .into_any_element()
    }

    pub(super) fn render_service_tier_control(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.selected_session()?;
        let model = self.model_for_session(session)?;
        if !super::runtime::catalog_allows_service_tier(super::runtime::catalog_entry_for(
            &self.model_catalogs,
            &session.provider,
            model,
        )) {
            return None;
        }
        let tier = session
            .service_tier
            .unwrap_or(waku_client::ServiceTier::Default);
        let next = waku_client::ServiceTier::ALL[(waku_client::ServiceTier::ALL
            .iter()
            .position(|candidate| *candidate == tier)
            .unwrap_or(0)
            + 1)
            % waku_client::ServiceTier::ALL.len()];
        let weak = cx.entity().downgrade();
        let key_weak = weak.clone();
        Some(
            div()
                .id("service-tier")
                .tab_index(0)
                .focus_visible(|style| style.border_1().border_color(Theme::current(cx).accent))
                .px(px(7.0))
                .h(px(24.0))
                .rounded(px(6.0))
                .text_size(px(11.5))
                .text_color(Theme::current(cx).text_secondary)
                .child(SharedString::from(format!(
                    "{}: {}",
                    tr!("models.service_tier"),
                    tier
                )))
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |this, cx| this.set_service_tier(next, cx));
                })
                .on_key_down(move |event: &KeyDownEvent, _, cx| {
                    if !event.keystroke.modifiers.modified()
                        && matches!(event.keystroke.key.as_str(), "enter" | "space")
                    {
                        let _ = key_weak.update(cx, |this, cx| this.set_service_tier(next, cx));
                        cx.stop_propagation();
                    }
                })
                .into_any_element(),
        )
    }

    pub(super) fn render_access_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let selected_mode = self
            .selected_session()
            .map(|session| session.runtime_mode)
            .unwrap_or(RuntimeMode::Ask);
        let weak = cx.entity().downgrade();
        let handle = self.menu_handle("runtime-mode", cx);
        dropdown_menu(
            MenuChip::new("runtime-mode")
                .icon(selected_mode.icon(), theme.text_tertiary)
                .label(selected_mode.label())
                .caret(false)
                .selected(handle.is_open()),
            "runtime-mode-menu",
            &handle,
            MenuAlign::AboveLeft,
            move |_| {
                RuntimeMode::ACCESS_OPTIONS
                    .into_iter()
                    .map(|option| {
                        let weak = weak.clone();
                        let selected = option == selected_mode;
                        MenuItem::custom(move |_, _| {
                            div()
                                .w(px(288.0))
                                .py(px(4.0))
                                .flex()
                                .items_center()
                                .gap(px(10.0))
                                .child(icon(option.icon(), 14.0, theme.text_tertiary))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .w_full()
                                                .truncate()
                                                .text_size(px(12.0))
                                                .font_weight(if selected {
                                                    FontWeight::SEMIBOLD
                                                } else {
                                                    FontWeight::MEDIUM
                                                })
                                                .text_color(theme.text)
                                                .child(option.label()),
                                        )
                                        .child(
                                            div()
                                                .w_full()
                                                .mt(px(2.0))
                                                .text_size(px(10.5))
                                                .line_height(px(14.0))
                                                .whitespace_normal()
                                                .text_color(theme.text_tertiary)
                                                .child(option.description()),
                                        ),
                                )
                                .when(selected, |element| {
                                    element.child(icon(
                                        "icons/check.svg",
                                        11.0,
                                        theme.text_tertiary,
                                    ))
                                })
                                .into_any_element()
                        })
                        .on_click(move |_, cx| {
                            let _ = weak.update(cx, |this, cx| this.set_runtime_mode(option, cx));
                        })
                    })
                    .collect()
            },
        )
    }

    pub(super) fn reveal_selected_picker_model(&self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let provider = &session.provider;
        let model = self.model_for_session(session);
        let models = visible_picker_models(
            &self.state.external_providers,
            &self.model_catalogs,
            &self.state.favorite_models,
            Some(provider),
            &self.model_picker_tab,
            "",
        );
        let index = models
            .iter()
            .position(|(candidate, item)| candidate == provider && model == Some(item.id.as_str()))
            .unwrap_or(0);
        self.model_picker_scroll.scroll_to_item(index);
    }

    pub(super) fn render_interaction_mode_control(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let mode = self
            .selected_session()
            .map(|session| session.interaction_mode)
            .unwrap_or_default();
        let interactive = true;
        let next_mode = if mode == InteractionMode::Plan {
            InteractionMode::Build
        } else {
            InteractionMode::Plan
        };
        let weak = cx.entity().downgrade();
        let key_weak = weak.clone();
        div()
            .id("interaction-mode")
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .h(px(24.0))
            .px(px(7.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(px(11.5))
            .line_height(px(14.0))
            .text_color(if mode == InteractionMode::Plan {
                theme.accent
            } else {
                theme.text_secondary
            })
            .child(icon(
                if mode == InteractionMode::Plan {
                    "icons/list.svg"
                } else {
                    "icons/wrench.svg"
                },
                10.5,
                if mode == InteractionMode::Plan {
                    theme.accent
                } else {
                    theme.text_tertiary
                },
            ))
            .child(mode.label())
            .when(interactive, |element| {
                element
                    .hover(|element| element.bg(theme.overlay))
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.set_interaction_mode(next_mode, cx);
                        });
                    })
                    .on_key_down(move |event: &KeyDownEvent, _, cx| {
                        if !event.keystroke.modifiers.modified()
                            && matches!(event.keystroke.key.as_str(), "enter" | "space")
                        {
                            let _ = key_weak.update(cx, |this, cx| {
                                this.set_interaction_mode(next_mode, cx);
                            });
                            cx.stop_propagation();
                        }
                    })
            })
            .into_any_element()
    }

    /// Stage files dropped onto the composer as attachment chips. The mention
    /// each chip will submit takes the autocomplete's form: relative to the
    /// project root when the file is inside it, absolute otherwise,
    /// directories with a trailing slash.
    pub(super) fn stage_dropped_files(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.stage_attachment_paths(paths.paths(), cx) {
            return;
        }
        let focus = self.composer.read(cx).focus();
        window.focus(&focus, cx);
    }

    fn stage_attachment_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> bool {
        if paths.is_empty() {
            return false;
        }
        let paths = paths.to_vec();
        let daemon = self.daemon.clone();
        let draft_owner = self.selected_composer_draft_key();
        cx.spawn(async move |waku, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let mut stored = Vec::with_capacity(paths.len());
                    for source_path in paths {
                        let (name, upload, image_bytes) =
                            attachment_upload_from_path(&source_path)?;
                        let is_image = image_bytes.is_some();
                        let preview_image = image_bytes.and_then(|bytes| {
                            image_preview::image_format_for_name(&name)
                                .map(|format| Arc::new(gpui::Image::from_bytes(format, bytes)))
                        });
                        let response = daemon.client().request(
                            Uuid::nil(),
                            Uuid::nil(),
                            waku_client::Command::ImportAttachment { name, upload },
                        )?;
                        let waku_client::ResponsePayload::AttachmentStored { attachment } =
                            response
                        else {
                            anyhow::bail!("the daemon returned an invalid attachment response");
                        };
                        stored.push((attachment, preview_image, is_image));
                    }
                    Ok::<_, anyhow::Error>(stored)
                })
                .await;
            let _ = waku.update(cx, |waku, cx| match result {
                Ok(stored) => {
                    if waku.selected_composer_draft_key() != draft_owner {
                        return;
                    }
                    let mut changed = false;
                    for (attachment, preview_image, is_image) in stored {
                        changed |= waku.stage_daemon_attachment(
                            attachment.path,
                            attachment.name,
                            attachment.is_dir,
                            is_image,
                            attachment.reference,
                            preview_image,
                        );
                    }
                    if changed {
                        waku.schedule_composer_draft_save(cx);
                        cx.notify();
                    }
                }
                Err(error) => {
                    waku.show_toast(error.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
        true
    }

    fn stage_daemon_attachment(
        &mut self,
        path: PathBuf,
        name: String,
        is_dir: bool,
        is_image: bool,
        reference: String,
        client_preview_image: Option<Arc<gpui::Image>>,
    ) -> bool {
        if self.composer_attachments.iter().any(|attachment| {
            attachment.path == path
                || attachment.blob_reference.as_deref() == Some(reference.as_str())
        }) {
            return false;
        }
        let mut mention = path.display().to_string();
        if is_dir && !mention.ends_with('/') {
            mention.push('/');
        }
        self.composer_attachments.push(ComposerAttachment {
            path,
            client_preview_image,
            mention,
            name: SharedString::from(name),
            is_dir,
            is_image,
            blob_reference: Some(reference),
        });
        true
    }

    /// Stage the clipboard's primary image/file representation. On-disk paths
    /// reuse drop handling immediately; raw image bytes are copied into Waku's
    /// durable blob store on the background executor before their chip appears.
    pub(super) fn stage_pasted_attachments(
        &mut self,
        entries: Vec<ClipboardEntry>,
        cx: &mut Context<Self>,
    ) {
        let mut paths = Vec::new();
        let mut images = Vec::new();
        for entry in entries {
            match entry {
                ClipboardEntry::Image(image) if !image.bytes.is_empty() => images.push(image),
                ClipboardEntry::ExternalPaths(external) => {
                    paths.extend(external.paths().iter().cloned())
                }
                ClipboardEntry::String(_) | ClipboardEntry::Image(_) => {}
            }
        }
        self.stage_attachment_paths(&paths, cx);
        if images.is_empty() {
            return;
        }

        let daemon = self.daemon.clone();
        let draft_owner = self.selected_composer_draft_key();
        cx.spawn(async move |waku, cx| {
            let stored = cx
                .background_executor()
                .spawn(async move {
                    let image_count = images.len();
                    images
                        .into_iter()
                        .enumerate()
                        .map(|(index, image)| {
                            let preview_image = Arc::new(image);
                            let bytes = preview_image.bytes.clone();
                            let response = daemon
                                .client()
                                .request(
                                    Uuid::nil(),
                                    Uuid::nil(),
                                    waku_client::Command::StoreBlob {
                                        mime_type: preview_image.format.mime_type().to_owned(),
                                        bytes,
                                    },
                                )
                                .map_err(|error| error.to_string())?;
                            let waku_client::ResponsePayload::BlobStored { reference, path } =
                                response
                            else {
                                return Err("the daemon returned an invalid blob response".into());
                            };
                            let extension = path
                                .extension()
                                .and_then(|extension| extension.to_str())
                                .unwrap_or("png");
                            let name = if image_count == 1 {
                                format!("image.{extension}")
                            } else {
                                format!("image-{}.{extension}", index + 1)
                            };
                            Ok::<_, String>((path, name, reference, preview_image))
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .await;
            let _ = waku.update(cx, |waku, cx| match stored {
                Ok(stored) => {
                    if waku.selected_composer_draft_key() != draft_owner {
                        return;
                    }
                    let mut staged = false;
                    for (path, name, reference, preview_image) in stored {
                        staged |= waku.stage_daemon_attachment(
                            path,
                            name,
                            false,
                            true,
                            reference,
                            Some(preview_image),
                        );
                    }
                    if staged {
                        waku.schedule_composer_draft_save(cx);
                        cx.notify();
                    }
                }
                Err(error) => {
                    waku.show_toast(tr!("errors.store_pasted_image", error = error));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The text and attachment presentation accepted from the composer. The
    /// exact provider prompt keeps its `@` mentions, while sent-message UI uses
    /// `display_content` and the retained attachment metadata.
    pub(super) fn submission_with_attachments(
        &mut self,
        prompt: &str,
        cx: &mut Context<Self>,
    ) -> Option<ComposerSubmission> {
        if self.execute_local_composer_command(prompt, cx) {
            return None;
        }
        for attachment in &self.composer_attachments {
            if let (Some(reference), Some(image)) = (
                attachment.blob_reference.as_ref(),
                attachment.client_preview_image.as_ref(),
            ) {
                self.remote_images
                    .borrow_mut()
                    .insert(reference.clone(), RemoteImageState::Ready(image.clone()));
            }
        }
        let attachments = self
            .composer_attachments
            .drain(..)
            .map(MessageAttachment::from)
            .collect::<Vec<_>>();
        let mentions = attachments
            .iter()
            .map(|attachment| attachment.mention.clone())
            .collect::<Vec<_>>();
        let submission = merged_submission(prompt, &mentions)?;
        let display_content = (!attachments.is_empty()).then(|| prompt.trim().to_owned());
        self.discard_current_composer_draft(cx);
        Some(ComposerSubmission {
            prompt: submission,
            display_content,
            attachments,
        })
    }

    pub(super) fn execute_local_composer_command(
        &mut self,
        _prompt: &str,
        _cx: &mut Context<Self>,
    ) -> bool {
        false
    }

    pub(super) fn restore_composer_submission(
        &mut self,
        submission: ComposerSubmission,
        cx: &mut Context<Self>,
    ) {
        self.composer_attachments = submission
            .attachments
            .into_iter()
            .map(ComposerAttachment::from)
            .collect();
        let content = submission.display_content.unwrap_or(submission.prompt);
        self.composer
            .update(cx, |input, cx| input.set_content(content, cx));
        self.schedule_composer_draft_save(cx);
        cx.notify();
    }

    /// The staged-attachment chips above the input: a thumbnail tile per
    /// image, a file-type icon and basename for everything else, each with a
    /// floating remove button — T3 Code's attachment row in graphite.
    fn render_composer_attachments(&self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let mut row = div()
            .px(px(14.0))
            .pt(px(2.0))
            .pb(px(8.0))
            .flex()
            .flex_wrap()
            .gap(px(8.0));
        for (index, attachment) in self.composer_attachments.iter().enumerate() {
            let menu = self.menu_handle(format!("composer-attachment-{index}-menu"), cx);
            let icon_path = if attachment.is_dir {
                "icons/folder.svg"
            } else {
                super::right_panel::file_icon_for_path(&attachment.mention)
            };
            let mut tile = div()
                .id(SharedString::from(format!("composer-attachment-{index}")))
                .relative()
                .w(px(64.0))
                .h(px(64.0))
                .rounded(px(8.0))
                .overflow_hidden()
                .border_1()
                .border_color(theme.border)
                .bg(theme.inset)
                .track_focus(menu.trigger_focus_handle())
                .tab_index(0)
                .focus_visible(|style| style.border_color(theme.accent))
                .tooltip(Tooltip::text(format!("@{}", attachment.mention)));
            let attachment_image = attachment.client_preview_image.clone().or_else(|| {
                attachment
                    .is_image
                    .then(|| {
                        attachment.blob_reference.as_deref().and_then(|reference| {
                            self.image_for_reference(
                                reference,
                                Some(&attachment.path),
                                Some(attachment.name.as_ref()),
                                cx,
                            )
                        })
                    })
                    .flatten()
            });
            let can_reveal = !self.daemon.is_remote();
            if attachment.is_image {
                if let Some(attachment_image) = attachment_image.as_ref() {
                    let preview_image = attachment_image.clone();
                    let preview_name = attachment.name.clone();
                    tile = tile.child(
                        div()
                            .id(SharedString::from(format!(
                                "composer-attachment-{index}-preview"
                            )))
                            .size_full()
                            .cursor_default()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_image_preview(
                                    preview_image.clone(),
                                    preview_name.clone(),
                                    window,
                                    cx,
                                );
                                cx.stop_propagation();
                            }))
                            .child(
                                img(attachment_image.clone())
                                    .size_full()
                                    .object_fit(ObjectFit::Cover),
                            ),
                    );
                } else {
                    tile = tile.child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("icons/file-types/image.svg", 16.0, theme.text_ghost)),
                    );
                }
            } else {
                tile = tile.child(
                    div()
                        .size_full()
                        .px(px(5.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(5.0))
                        .child(icon(icon_path, 16.0, theme.text_tertiary))
                        .child(
                            div().w_full().flex().justify_center().child(
                                div()
                                    .max_w_full()
                                    .truncate()
                                    .text_size(px(8.5))
                                    .text_color(theme.text_tertiary)
                                    .child(attachment.name.clone()),
                            ),
                        ),
                );
            }
            let key_menu = menu.clone();
            let key_image = attachment_image.clone();
            let key_name = attachment.name.clone();
            let is_image = attachment.is_image;
            tile = tile.on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let key = event.keystroke.key.as_str();
                if is_image
                    && matches!(key, "enter" | "space")
                    && let Some(key_image) = key_image.as_ref()
                {
                    this.open_image_preview(key_image.clone(), key_name.clone(), window, cx);
                    cx.stop_propagation();
                } else if key == "f10" && event.keystroke.modifiers.shift {
                    key_menu.open_context_menu(window, cx);
                    cx.stop_propagation();
                }
            }));
            let tile = tile.child(
                div()
                    .id(SharedString::from(format!(
                        "composer-attachment-remove-{index}"
                    )))
                    .absolute()
                    .top(px(3.0))
                    .right(px(3.0))
                    .w(px(16.0))
                    .h(px(16.0))
                    .tab_index(0)
                    .rounded(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .bg(theme.canvas.opacity(0.8))
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .hover(|element| element.bg(theme.canvas.opacity(0.95)))
                    .active(|element| element.opacity(0.8))
                    .child(icon("icons/x.svg", 9.0, theme.text_secondary))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        if index < this.composer_attachments.len() {
                            this.composer_attachments.remove(index);
                            this.schedule_composer_draft_save(cx);
                            cx.notify();
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            if index < this.composer_attachments.len() {
                                this.composer_attachments.remove(index);
                                this.schedule_composer_draft_save(cx);
                                cx.notify();
                            }
                            cx.stop_propagation();
                        }
                    })),
            );
            let reveal_path = attachment.path.clone();
            row = row.child(context_menu(
                tile,
                SharedString::from(format!("composer-attachment-{index}-context-menu")),
                &menu,
                move |_| image_preview::attachment_menu_items(reveal_path.clone(), can_reveal),
            ));
        }
        row
    }

    /// The pending follow-up queue between the transcript and the composer: a
    /// single card tucked against the composer's top edge, one row per queued
    /// message. A row pulls its text back into the composer on click and
    /// carries steer/remove/more controls on the right.
    pub(super) fn render_queued_messages(&self, cx: &mut Context<Self>) -> Option<Div> {
        let session_id = self.state.selected_session?;
        let session = self.selected_session()?;
        if session.queued_messages.is_empty() {
            return None;
        }
        let theme = Theme::current(cx);
        let steerable = session.is_busy()
            && session.status != SessionStatus::Connecting
            && self
                .runtimes
                .get(&session.id)
                .is_some_and(|runtime| runtime.driver.supports_steer());
        let mut list = div().flex().flex_col().py(px(4.0));
        for message in &session.queued_messages {
            let message_id = message.id;
            let content = if message.visible_content().trim().is_empty() {
                message
                    .attachments
                    .iter()
                    .map(|attachment| attachment.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                message.visible_content().to_owned()
            };
            let steer_control = steerable.then(|| {
                div()
                    .id(SharedString::from(format!(
                        "queued-message-steer-{message_id}"
                    )))
                    .h(px(24.0))
                    .px(px(7.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .hover(|element| element.bg(theme.overlay_strong))
                    .active(|element| element.opacity(0.8))
                    .text_size(px(11.5))
                    .text_color(theme.text_secondary)
                    .child(icon(
                        "icons/corner-down-right.svg",
                        11.0,
                        theme.text_secondary,
                    ))
                    .child(tr!("composer.steer"))
                    .tooltip(Tooltip::text(tr!("composer.steer_current")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.steer_queued_message(session_id, message_id, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.steer_queued_message(session_id, message_id, cx);
                            cx.stop_propagation();
                        }
                    }))
            });
            let menu_handle = self.menu_handle(format!("queued-message-menu-{message_id}"), cx);
            let menu_open = menu_handle.is_open();
            let weak = cx.entity().downgrade();
            let more_control = dropdown_menu(
                div()
                    .id(SharedString::from(format!(
                        "queued-message-more-{message_id}"
                    )))
                    .w(px(24.0))
                    .h(px(24.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_default()
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .when(menu_open, |element| element.bg(theme.overlay_strong))
                    .hover(|element| element.bg(theme.overlay_strong))
                    .active(|element| element.opacity(0.8))
                    .child(icon("icons/ellipsis.svg", 12.5, theme.text_secondary)),
                SharedString::from(format!("queued-message-more-menu-{message_id}")),
                &menu_handle,
                MenuAlign::BelowRight,
                move |_| {
                    let edit_weak = weak.clone();
                    let remove_weak = weak.clone();
                    vec![
                        MenuItem::new(tr!("composer.edit_in_composer"), move |window, cx| {
                            let _ = edit_weak.update(cx, |this, cx| {
                                this.edit_queued_message(session_id, message_id, window, cx);
                            });
                        })
                        .icon("icons/pencil.svg"),
                        MenuItem::new(tr!("composer.remove_followup"), move |_, cx| {
                            let _ = remove_weak.update(cx, |this, cx| {
                                this.remove_queued_message(session_id, message_id, cx);
                            });
                        })
                        .icon("icons/trash.svg"),
                    ]
                },
            );
            list = list.child(
                div()
                    .id(SharedString::from(format!("queued-message-{message_id}")))
                    .h(px(30.0))
                    .pl(px(12.0))
                    .pr(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .cursor_default()
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.accent))
                    .hover(|element| element.bg(theme.overlay))
                    .tooltip(Tooltip::text(tr!("composer.edit_in_composer")))
                    .child(icon("icons/queue.svg", 12.0, theme.text_tertiary))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .text_color(theme.text)
                            .child(SharedString::from(content)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .children(steer_control)
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "queued-message-remove-{message_id}"
                                    )))
                                    .w(px(24.0))
                                    .h(px(24.0))
                                    .rounded(px(6.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_default()
                                    .tab_index(0)
                                    .focus_visible(|style| {
                                        style.border_1().border_color(theme.accent)
                                    })
                                    .hover(|element| element.bg(theme.overlay_strong))
                                    .active(|element| element.opacity(0.8))
                                    .child(icon("icons/trash.svg", 12.0, theme.text_secondary))
                                    .tooltip(Tooltip::text(tr!("composer.remove_followup")))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.remove_queued_message(session_id, message_id, cx);
                                    }))
                                    .on_key_down(cx.listener(
                                        move |this, event: &KeyDownEvent, _, cx| {
                                            if matches!(
                                                event.keystroke.key.as_str(),
                                                "enter" | "space"
                                            ) {
                                                this.remove_queued_message(
                                                    session_id, message_id, cx,
                                                );
                                                cx.stop_propagation();
                                            }
                                        },
                                    )),
                            )
                            .child(more_control),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.edit_queued_message(session_id, message_id, window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.edit_queued_message(session_id, message_id, window, cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        Some(
            div().flex_none().px(px(20.0)).child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .px(px(14.0))
                    .child(
                        div()
                            .rounded_tl(px(12.0))
                            .rounded_tr(px(12.0))
                            .border_t_1()
                            .border_l_1()
                            .border_r_1()
                            .border_color(theme.border)
                            .bg(theme.composer)
                            // Row hover fills are full-width rectangles; clip
                            // them to the card's rounded corners.
                            .overflow_hidden()
                            .child(list),
                    ),
            ),
        )
    }

    pub(super) fn render_composer(&self, window: &Window, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let session = self.selected_session();
        let preparing = session.is_some_and(|session| {
            self.submission_preparations.contains(&session.id)
                || self.response_fork_preparations.contains_key(&session.id)
        });
        let submit_action =
            composer_submit_action(session.map(|session| session.status), preparing);
        let escape_stop_armed = session.is_some_and(|session| {
            self.escape_stop_confirmation
                .is_armed_for(EscapeStopTarget::for_session(session), Instant::now())
        });
        let has_draft = !self.composer.read(cx).content().trim().is_empty()
            || !self.composer_attachments.is_empty();
        let autocomplete = self.render_composer_autocomplete(window, cx);
        let autocomplete_open = autocomplete.is_some();
        // Files dragged in from the OS light the card up as a drop target and
        // stage as attachment chips. The wash arrives pre-blended because a
        // drag-over refinement replaces the card's fill rather than
        // compositing over it.
        let drop_wash = theme.composer.blend(theme.overlay_strong);
        let drop_ring = theme.accent.opacity(0.7);
        div().flex_none().px(px(20.0)).child(
            div()
                .w_full()
                .max_w(px(CONTENT_MAX_WIDTH))
                .mx_auto()
                .rounded(px(13.0))
                .border_1()
                .border_color(theme.border)
                .bg(theme.composer)
                // Horizontal insets live on each row (and inside the field's
                // scroll viewport, via `padding_x`) rather than on the card,
                // so the field's overlay scrollbar can hug the card's edge.
                .py(px(10.0))
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.bg(drop_wash).border_color(drop_ring)
                })
                .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                    this.stage_dropped_files(paths, window, cx);
                }))
                // Anchor for the bounds probe the autocomplete popup aligns to.
                .relative()
                .child(super::autocomplete::composer_card_bounds_probe(
                    self.composer_autocomplete.card_bounds_cell(),
                ))
                // Only while the popup is open: the key context routes the
                // arrows, `enter`, `tab` and `escape` here as actions, out
                // from under the focused field. When it closes, the context
                // disappears with it and `enter` submits again.
                .when(autocomplete_open, |card| {
                    card.key_context("ComposerAutocomplete")
                        .on_action(cx.listener(|this, _: &SelectNextEntry, window, cx| {
                            this.move_autocomplete_highlight("down", window, cx);
                        }))
                        .on_action(cx.listener(|this, _: &SelectPreviousEntry, window, cx| {
                            this.move_autocomplete_highlight("up", window, cx);
                        }))
                        .on_action(cx.listener(|this, _: &ConfirmEntry, window, cx| {
                            this.accept_autocomplete(None, window, cx);
                        }))
                        .on_action(cx.listener(|this, _: &DismissMenu, _, cx| {
                            this.dismiss_autocomplete(cx);
                        }))
                })
                .children(autocomplete)
                .when(!self.composer_attachments.is_empty(), |card| {
                    card.child(self.render_composer_attachments(cx))
                })
                .child(div().pt(px(2.0)).child(self.composer.clone()))
                .child(
                    div()
                        .mt(px(8.0))
                        .px(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .text_size(px(11.5))
                        .line_height(px(14.0))
                        .child(self.render_provider_model_control(cx))
                        .children(self.render_service_tier_control(cx))
                        .child(self.render_access_control(cx))
                        .child(self.render_interaction_mode_control(cx))
                        .child(div().flex_1())
                        .child(match submit_action {
                            ComposerSubmitAction::Preparing => div()
                                .id("send-or-stop")
                                .w(px(26.0))
                                .h(px(26.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_default()
                                .bg(theme.overlay_strong)
                                .child(motion::spin(icon(
                                    "icons/loader-circle.svg",
                                    15.0,
                                    theme.text_secondary,
                                )))
                                .tooltip(Tooltip::text(tr!("composer.preparing_task"))),
                            ComposerSubmitAction::Stop => div()
                                .id("working-actions")
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .id("send-or-stop")
                                        .w(px(26.0))
                                        .h(px(26.0))
                                        .rounded_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_default()
                                        .bg(theme.overlay_strong)
                                        .hover(|element| element.bg(theme.danger_soft))
                                        .active(|element| element.opacity(0.8))
                                        .when(escape_stop_armed, |element| {
                                            element.child(
                                                div()
                                                    .text_size(px(10.0))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme.text)
                                                    .child("Esc"),
                                            )
                                        })
                                        .when(!escape_stop_armed, |element| {
                                            element.child(icon("icons/stop.svg", 18.0, theme.text))
                                        })
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.cancel_turn(cx);
                                        })),
                                )
                                .when(has_draft, |element| {
                                    element.child(
                                        div()
                                            .id("queue-follow-up")
                                            .w(px(26.0))
                                            .h(px(26.0))
                                            .rounded_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .cursor_default()
                                            .bg(theme.inverse)
                                            .hover(|element| element.opacity(0.9))
                                            .active(|element| element.opacity(0.8))
                                            .child(icon(
                                                "icons/arrow-up.svg",
                                                16.0,
                                                theme.on_inverse,
                                            ))
                                            .tooltip(Tooltip::text(tr!("composer.queue_followup")))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                let prompt =
                                                    this.composer.read(cx).content().to_owned();
                                                if let Some(submission) =
                                                    this.submission_with_attachments(&prompt, cx)
                                                {
                                                    this.composer
                                                        .update(cx, |input, cx| input.clear(cx));
                                                    this.submit_composer_submission(submission, cx);
                                                }
                                            })),
                                    )
                                }),
                            ComposerSubmitAction::Send => div()
                                .id("send-or-stop")
                                .w(px(26.0))
                                .h(px(26.0))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(if has_draft {
                                    theme.inverse
                                } else {
                                    theme.overlay_strong
                                })
                                .when(has_draft, |element| {
                                    element
                                        .cursor_default()
                                        .hover(|element| element.opacity(0.9))
                                        .active(|element| element.opacity(0.8))
                                })
                                .child(icon(
                                    "icons/arrow-up.svg",
                                    16.0,
                                    if has_draft {
                                        theme.on_inverse
                                    } else {
                                        theme.text_ghost
                                    },
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let prompt = this.composer.read(cx).content().to_owned();
                                    if let Some(submission) =
                                        this.submission_with_attachments(&prompt, cx)
                                    {
                                        this.composer.update(cx, |input, cx| input.clear(cx));
                                        this.submit_composer_submission(submission, cx);
                                    }
                                })),
                        }),
                ),
        )
    }

    fn render_branch_selector(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::current(cx);
        let session = self.selected_session()?;
        let workspace = session.workspace.clone();
        let workspace_path = self.workspace_path_for_session(session)?.to_path_buf();
        self.selected_project()
            .filter(|project| !project.is_projectless())?;
        let branch_enabled = !session.is_busy() && !self.branch_operation_pending;
        let planned_worktree = matches!(workspace, SessionWorkspace::NewWorktree { .. });
        let snapshot = self.branch_snapshot_for_workspace(&workspace_path, cx)?;
        let selected_branch = match &workspace {
            SessionWorkspace::Local => snapshot.display_branch().map(str::to_owned),
            SessionWorkspace::NewWorktree { base_branch } => base_branch
                .clone()
                .or_else(|| snapshot.default_branch.clone())
                .or_else(|| snapshot.display_branch().map(str::to_owned)),
            SessionWorkspace::Worktree { branch, .. } => snapshot
                .current
                .clone()
                .or_else(|| Some(branch.clone()))
                .or_else(|| snapshot.detached_head.clone()),
        }
        .unwrap_or_else(|| tr!("branches.detached_head"));

        let weak = cx.entity().downgrade();
        let search = self.branch_search.clone();
        let create_input = self.branch_create_input.clone();
        let search_focus = search.read(cx).focus_handle(cx);
        let handle = {
            let toggle_weak = weak.clone();
            let reset_search = search.clone();
            let reset_create = create_input.clone();
            let picker_focus = search_focus.clone();
            self.menu_handle_with(BRANCH_PICKER_MENU_ID, cx, move |open, window, cx| {
                let _ = toggle_weak.update(cx, |this, cx| {
                    if open {
                        this.branch_picker_mode = BranchPickerMode::Browse;
                        this.branch_picker_highlight = None;
                        let project_name = this
                            .selected_project()
                            .map(Project::display_name)
                            .unwrap_or_else(|| tr!("project.project_lower"));
                        reset_search.update(cx, |input, cx| {
                            input.set_placeholder(
                                tr!("branches.search_project", project = project_name),
                                cx,
                            );
                            input.clear(cx);
                        });
                        reset_create.update(cx, |input, cx| input.clear(cx));
                        this.refresh_selected_branch_snapshot(cx);
                    } else {
                        this.branch_picker_mode = BranchPickerMode::Browse;
                        let focus = this.composer_focus(cx);
                        window.focus(&focus, cx);
                    }
                    cx.notify();
                });
                if open {
                    let picker_focus = picker_focus.clone();
                    window.on_next_frame(move |window, _| {
                        window.on_next_frame(move |window, cx| window.focus(&picker_focus, cx));
                    });
                }
            })
        };

        let trigger = MenuChip::new("workspace-branch")
            .icon("icons/git-branch.svg", theme.text_tertiary)
            .label(if self.branch_operation_pending {
                tr!("branches.switching")
            } else {
                selected_branch.clone()
            })
            .caret(false)
            .disabled(!branch_enabled)
            .selected(branch_enabled && handle.is_open())
            .max_w(px(210.0));
        if !branch_enabled {
            return Some(trigger.into_any_element());
        }

        let normalized_query = self
            .branch_search
            .read(cx)
            .content()
            .trim()
            .to_ascii_lowercase();
        let visible_branches = Rc::new(
            if handle.is_open() && self.branch_picker_mode == BranchPickerMode::Browse {
                visible_branch_entries(&snapshot.branches, &selected_branch, &normalized_query)
            } else {
                Vec::new()
            },
        );
        let allow_create = !planned_worktree;
        let actions = Rc::new(
            visible_branches
                .iter()
                .filter(|branch| planned_worktree || !branch.checked_out_elsewhere)
                .map(|branch| BranchPickerAction::Checkout(branch.name.clone()))
                .chain(allow_create.then_some(BranchPickerAction::Create))
                .collect::<Vec<_>>(),
        );
        let highlight = self
            .branch_picker_highlight
            .filter(|index| *index < actions.len());
        let mode = self.branch_picker_mode;
        if handle.is_open() && mode == BranchPickerMode::Browse {
            self.sync_branch_picker_rows(&visible_branches);
        }
        let branch_list = self.branch_picker_list_state.clone();

        Some(popover(
            trigger,
            &handle,
            MenuAlign::AboveLeft,
            move |popover, _window, _cx| {
                let popover = popover.clone();
                let next_actions = actions.clone();
                let previous_actions = actions.clone();
                let confirm_actions = actions.clone();
                let next_weak = weak.clone();
                let previous_weak = weak.clone();
                let confirm_weak = weak.clone();
                let confirm_popover = popover.clone();

                let body = if mode == BranchPickerMode::Create {
                    div()
                        .w_full()
                        .p(px(14.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(icon("icons/plus.svg", 14.0, theme.text_secondary))
                                .child(tr!("branches.create_and_checkout")),
                        )
                        .child(
                            div()
                                .mt(px(12.0))
                                .h(px(36.0))
                                .px(px(10.0))
                                .rounded(px(9.0))
                                .border_1()
                                .border_color(theme.border_strong)
                                .bg(theme.surface)
                                .flex()
                                .items_center()
                                .child(div().flex_1().min_w_0().child(create_input.clone())),
                        )
                        .child(
                            div()
                                .mt(px(9.0))
                                .text_size(px(10.5))
                                .text_color(theme.text_tertiary)
                                .child(tr!("branches.create_hint")),
                        )
                        .into_any_element()
                } else {
                    let rows = if visible_branches.is_empty() {
                        div()
                            .id("branch-picker-list-empty")
                            .h(px(64.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.5))
                            .text_color(theme.text_ghost)
                            .child(tr!("branches.none_found"))
                            .into_any_element()
                    } else {
                        let list_branches = visible_branches.clone();
                        let list_actions = actions.clone();
                        let list_selected_branch = selected_branch.clone();
                        let list_weak = weak.clone();
                        let list_popover = popover.clone();
                        let height =
                            (visible_branches.len() as f32 * BRANCH_PICKER_ROW_HEIGHT).min(260.0);
                        div()
                            .id("branch-picker-list")
                            .w_full()
                            .h(px(height))
                            .flex_none()
                            .px(px(4.0))
                            .child(
                                list(branch_list.clone(), move |index, _window, _cx| {
                                    let Some(branch) = list_branches.get(index) else {
                                        return div().into_any_element();
                                    };
                                    let selected = branch.name == list_selected_branch;
                                    let disabled =
                                        branch.checked_out_elsewhere && !planned_worktree;
                                    let highlighted = highlight
                                        .and_then(|index| list_actions.get(index))
                                        .is_some_and(|action| {
                                            matches!(
                                                action,
                                                BranchPickerAction::Checkout(name)
                                                    if name == &branch.name
                                            )
                                        });
                                    let color = if disabled {
                                        theme.text_ghost
                                    } else {
                                        theme.text
                                    };
                                    let row = div()
                                        .id(SharedString::from(format!(
                                            "branch-row-{}",
                                            branch.name
                                        )))
                                        .w_full()
                                        .h(px(BRANCH_PICKER_ROW_HEIGHT))
                                        .px(px(8.0))
                                        .rounded(px(6.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .cursor_default()
                                        .when(highlighted, |element| {
                                            element.bg(theme.overlay_strong)
                                        })
                                        .when(!disabled, |element| {
                                            element
                                                .hover(|element| element.bg(theme.overlay))
                                                .active(|element| element.opacity(0.85))
                                        })
                                        .child(icon("icons/git-branch.svg", 12.0, color))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .flex_1()
                                                .truncate()
                                                .text_size(px(11.5))
                                                .line_height(px(15.0))
                                                .text_color(color)
                                                .child(SharedString::from(branch.name.clone())),
                                        )
                                        .when(selected, |element| {
                                            element.child(icon(
                                                "icons/check.svg",
                                                11.0,
                                                theme.text_secondary,
                                            ))
                                        });
                                    if disabled {
                                        row.into_any_element()
                                    } else {
                                        let branch_name = branch.name.clone();
                                        let select_weak = list_weak.clone();
                                        let select_popover = list_popover.clone();
                                        row.on_click(move |_, window, cx| {
                                            let should_close = select_weak
                                                .update(cx, |this, cx| {
                                                    this.choose_workspace_branch(
                                                        branch_name.clone(),
                                                        cx,
                                                    )
                                                })
                                                .unwrap_or(false);
                                            if should_close {
                                                select_popover.close(window, cx);
                                                window.refresh();
                                            }
                                        })
                                        .into_any_element()
                                    }
                                })
                                .size_full(),
                            )
                            .into_any_element()
                    };

                    let create_row = allow_create.then(|| {
                        let create_weak = weak.clone();
                        div()
                            .id("create-workspace-branch")
                            .mx(px(4.0))
                            .h(px(BRANCH_PICKER_ROW_HEIGHT))
                            .px(px(8.0))
                            .rounded(px(6.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .cursor_default()
                            .when(
                                highlight.and_then(|index| actions.get(index))
                                    == Some(&BranchPickerAction::Create),
                                |element| element.bg(theme.overlay_strong),
                            )
                            .hover(|element| element.bg(theme.overlay))
                            .active(|element| element.opacity(0.85))
                            .child(icon("icons/plus.svg", 12.0, theme.text_secondary))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .line_height(px(15.0))
                                    .text_color(theme.text)
                                    .child(tr!("branches.create_and_checkout_ellipsis")),
                            )
                            .on_click(move |_, window, cx| {
                                let _ = create_weak.update(cx, |this, cx| {
                                    this.begin_branch_creation(window, cx);
                                });
                            })
                    });

                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .h(px(52.0))
                                .px(px(12.0))
                                .pt(px(10.0))
                                .pb(px(8.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(34.0))
                                        .px(px(10.0))
                                        .rounded(px(9.0))
                                        .bg(theme.surface)
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(icon("icons/search.svg", 15.0, theme.text_secondary))
                                        .child(div().flex_1().min_w_0().child(search.clone())),
                                ),
                        )
                        .child(
                            div()
                                .px(px(14.0))
                                .pt(px(3.0))
                                .pb(px(7.0))
                                .text_size(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_tertiary)
                                .child(tr!("branches.title")),
                        )
                        .child(rows)
                        .when_some(create_row, |element, create_row| {
                            element
                                .child(div().mx(px(6.0)).my(px(4.0)).h(px(1.0)).bg(theme.border))
                                .child(create_row)
                                .child(div().h(px(4.0)))
                        })
                        .into_any_element()
                };

                div()
                    .w(px(360.0))
                    .max_h(px(390.0))
                    .rounded(px(13.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.raised)
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_action(move |_: &SelectNextEntry, _, cx| {
                        let _ = next_weak.update(cx, |this, cx| {
                            this.move_branch_picker_highlight("down", &next_actions, cx);
                        });
                    })
                    .on_action(move |_: &SelectPreviousEntry, _, cx| {
                        let _ = previous_weak.update(cx, |this, cx| {
                            this.move_branch_picker_highlight("up", &previous_actions, cx);
                        });
                    })
                    .on_action(move |_: &ConfirmEntry, window, cx| {
                        let should_close = confirm_weak
                            .update(cx, |this, cx| {
                                this.confirm_branch_picker_action(&confirm_actions, window, cx)
                            })
                            .unwrap_or(false);
                        if should_close {
                            confirm_popover.close(window, cx);
                            window.refresh();
                        }
                    })
                    .child(body)
                    .into_any_element()
            },
        ))
    }

    pub(super) fn render_workspace_footer(&mut self, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let selected_project_id = self.state.selected_project;
        let projectless_selected = self.selected_project().is_some_and(Project::is_projectless);
        let project_name = self
            .selected_project()
            .map(|project| {
                if project.is_projectless() {
                    tr!("project.choose_project")
                } else {
                    project.display_name()
                }
            })
            .unwrap_or_else(|| tr!("project.choose_project"));
        let can_configure_workspace = self
            .selected_session()
            .is_some_and(|session| !session.has_started() && !session.is_busy());

        let project_handle = self.menu_handle("workspace-project", cx);
        let project_trigger = MenuChip::new("workspace-project")
            .icon("icons/folder.svg", theme.text_tertiary)
            .label(project_name)
            .caret(false)
            .disabled(!can_configure_workspace)
            .selected(can_configure_workspace && project_handle.is_open())
            .max_w(px(190.0));
        let project_selector = if can_configure_workspace {
            let project_options = self
                .state
                .projects
                .iter()
                .filter(|project| !project.is_projectless())
                .filter(|project| Some(project.id) == selected_project_id)
                .chain(
                    self.state
                        .projects
                        .iter()
                        .filter(|project| !project.is_projectless())
                        .filter(|project| Some(project.id) != selected_project_id),
                )
                .map(|project| (project.id, project.display_name()))
                .collect::<Vec<_>>();
            let weak = cx.entity().downgrade();
            dropdown_menu(
                project_trigger,
                "workspace-project-menu",
                &project_handle,
                MenuAlign::AboveLeft,
                move |_| {
                    let mut items = project_options
                        .clone()
                        .into_iter()
                        .map(|(project_id, project_name)| {
                            let weak = weak.clone();
                            MenuItem::new(project_name, move |_, cx| {
                                if Some(project_id) != selected_project_id {
                                    let _ = weak.update(cx, |this, cx| {
                                        this.select_project_from_composer(project_id, cx);
                                    });
                                }
                            })
                            .selected(Some(project_id) == selected_project_id)
                        })
                        .collect::<Vec<_>>();
                    if !items.is_empty() {
                        items.push(MenuItem::Separator);
                    }
                    let add_project = weak.clone();
                    items.push(
                        MenuItem::new(tr!("project.new_project"), move |_, cx| {
                            let _ = add_project.update(cx, |this, cx| this.add_project(cx));
                        })
                        .icon("icons/folder-new.svg"),
                    );
                    let projectless = weak.clone();
                    items.push(
                        MenuItem::new(tr!("project.no_project"), move |_, cx| {
                            let _ = projectless.update(cx, |this, cx| {
                                if !this.selected_project().is_some_and(Project::is_projectless) {
                                    this.create_projectless_session_from_composer(cx);
                                }
                            });
                        })
                        .icon("icons/x.svg")
                        .selected(projectless_selected),
                    );
                    items
                },
            )
        } else {
            project_trigger.into_any_element()
        };

        let workspace = self
            .selected_session()
            .map(|session| session.workspace.clone())
            .unwrap_or_default();
        let workspace_label = match &workspace {
            SessionWorkspace::Local => SharedString::from(tr!("workspace.local")),
            SessionWorkspace::NewWorktree { .. } => {
                SharedString::from(tr!("workspace.new_worktree"))
            }
            SessionWorkspace::Worktree { branch, .. } => SharedString::from(branch.clone()),
        };
        let workspace_icon = if workspace.is_local() {
            "icons/laptop.svg"
        } else {
            "icons/fork.svg"
        };
        let worktree_handle = self.menu_handle("workspace-worktree", cx);
        let worktree_trigger = MenuChip::new("workspace-worktree")
            .icon(workspace_icon, theme.text_tertiary)
            .label(workspace_label)
            .caret(false)
            .disabled(!can_configure_workspace)
            .selected(can_configure_workspace && worktree_handle.is_open())
            .max_w(px(180.0));
        let worktree_selector = if can_configure_workspace {
            let local_selected = workspace.is_local();
            let worktree_selected = workspace.is_worktree();
            let weak = cx.entity().downgrade();
            dropdown_menu(
                worktree_trigger,
                "workspace-worktree-menu",
                &worktree_handle,
                MenuAlign::AboveLeft,
                move |_| {
                    let local = weak.clone();
                    let worktree = weak.clone();
                    vec![
                        MenuItem::Header(tr!("workspace.work_in").into()),
                        MenuItem::new(tr!("workspace.local"), move |_, cx| {
                            let _ = local.update(cx, |this, cx| {
                                this.select_workspace(SessionWorkspace::Local, cx);
                            });
                        })
                        .icon("icons/laptop.svg")
                        .selected(local_selected),
                        MenuItem::new(tr!("workspace.new_worktree"), move |_, cx| {
                            let _ = worktree.update(cx, |this, cx| {
                                this.select_workspace(
                                    SessionWorkspace::NewWorktree { base_branch: None },
                                    cx,
                                );
                            });
                        })
                        .icon("icons/fork.svg")
                        .selected(worktree_selected)
                        .disabled(projectless_selected),
                    ]
                },
            )
        } else {
            worktree_trigger.into_any_element()
        };

        let branch_selector = self.render_branch_selector(cx);

        let usage_meter = self.render_usage_meter(cx);
        div()
            .flex_none()
            .px(px(20.0))
            .pb(px(8.0))
            .pt(px(4.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .h(px(28.0))
                    // The chip contributes 7px, lining its icon up with the
                    // composer's 10px padding plus the controls' 7px inset.
                    .pl(px(10.0))
                    .pr(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .tab_index(0)
                    .tab_group()
                    .tab_stop(false)
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .child(project_selector)
                    .child(worktree_selector)
                    .children(branch_selector)
                    .child(div().flex_1())
                    .children(usage_meter),
            )
    }
}

/// Branches matching the search, with the selected branch pinned first and
/// every other row sorted by name. Disabled worktree-owned rows stay in the
/// result; the UI needs to explain why Git cannot switch to them.
pub(super) fn visible_branch_entries(
    branches: &[crate::git_branch::BranchEntry],
    selected_branch: &str,
    normalized_query: &str,
) -> Vec<crate::git_branch::BranchEntry> {
    let normalized_query = normalized_query.to_ascii_lowercase();
    let mut visible = branches
        .iter()
        .filter(|branch| {
            normalized_query
                .split_whitespace()
                .all(|token| branch.name.to_ascii_lowercase().contains(token))
        })
        .cloned()
        .collect::<Vec<_>>();
    visible.sort_by(|left, right| {
        let left_selected = left.name == selected_branch;
        let right_selected = right.name == selected_branch;
        right_selected
            .cmp(&left_selected)
            .then_with(|| left.name.cmp(&right.name))
    });
    visible
}

/// The mention a dropped file submits: relative to the project root when the
/// file is inside it, absolute otherwise, directories with a trailing slash —
/// the same form the `@` autocomplete inserts. Dropping the root itself keeps
/// the absolute path rather than producing an empty mention.
// Base64 keeps the authenticated JSON transport browser-compatible but adds
// one third of wire overhead. Stay comfortably below tungstenite's default
// message limit until uploads move to a streaming content endpoint.
const MAX_ATTACHMENT_BYTES: u64 = waku_client::attachments::MAX_ATTACHMENT_BYTES as u64;

/// Reads a client-local drop into an upload payload. This is the explicit
/// client/daemon boundary: none of these source paths are persisted or handed
/// to a provider.
fn attachment_upload_from_path(
    source: &Path,
) -> anyhow::Result<(
    String,
    waku_client::attachments::AttachmentUpload,
    Option<Vec<u8>>,
)> {
    let metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("could not read attachment {}", source.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!(
            "symbolic-link attachments are not supported: {}",
            source.display()
        );
    }
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("attachment has no file name: {}", source.display()))?
        .to_owned();
    if metadata.is_file() {
        if metadata.len() > MAX_ATTACHMENT_BYTES {
            anyhow::bail!("attachment is larger than 32 MB: {}", source.display());
        }
        let bytes = std::fs::read(source)
            .with_context(|| format!("could not read attachment {}", source.display()))?;
        let is_image = is_image_attachment_path(source);
        return Ok((
            name,
            waku_client::attachments::AttachmentUpload::File {
                data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            },
            is_image.then_some(bytes),
        ));
    }
    if !metadata.is_dir() {
        anyhow::bail!(
            "attachment is not a file or directory: {}",
            source.display()
        );
    }

    let mut pending = vec![source.to_path_buf()];
    let mut entries = Vec::new();
    let mut total_bytes = 0u64;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).with_context(|| {
            format!(
                "could not read attachment directory {}",
                directory.display()
            )
        })? {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if entries.len() >= waku_client::attachments::MAX_ATTACHMENT_FILES {
                anyhow::bail!(
                    "attachment directory contains more than {} files",
                    waku_client::attachments::MAX_ATTACHMENT_FILES
                );
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > MAX_ATTACHMENT_BYTES {
                anyhow::bail!("attachment directory is larger than 32 MB");
            }
            let relative_path = path
                .strip_prefix(source)
                .context("attachment entry escaped its source directory")?
                .to_path_buf();
            let bytes = std::fs::read(&path)
                .with_context(|| format!("could not read attachment {}", path.display()))?;
            entries.push(waku_client::attachments::AttachmentUploadEntry {
                relative_path,
                data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            });
        }
    }
    Ok((
        name,
        waku_client::attachments::AttachmentUpload::Directory { entries },
        None,
    ))
}

#[cfg(test)]
pub(super) fn dropped_file_mention(
    root: Option<&std::path::Path>,
    path: &std::path::Path,
    is_dir: bool,
) -> String {
    let mention = root
        .and_then(|root| path.strip_prefix(root).ok())
        .filter(|relative| !relative.as_os_str().is_empty())
        .unwrap_or(path)
        .display()
        .to_string();
    if is_dir && !mention.ends_with('/') {
        format!("{mention}/")
    } else {
        mention
    }
}

fn is_image_attachment_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png"
                    | "jpg"
                    | "jpeg"
                    | "gif"
                    | "webp"
                    | "bmp"
                    | "svg"
                    | "tif"
                    | "tiff"
                    | "ico"
                    | "pnm"
                    | "pbm"
                    | "pgm"
                    | "ppm"
            )
        })
}

/// The prompt a submission sends: the typed text plus one `@` mention per
/// staged attachment, appended at the end the way T3 Code appends dropped
/// files. `None` means there is nothing to send.
pub(super) fn merged_submission(prompt: &str, mentions: &[String]) -> Option<String> {
    let mentions = mentions
        .iter()
        .map(|mention| format!("@{mention}"))
        .collect::<Vec<_>>()
        .join(" ");
    let prompt = prompt.trim();
    match (prompt.is_empty(), mentions.is_empty()) {
        (true, true) => None,
        (false, true) => Some(prompt.to_owned()),
        (true, false) => Some(mentions),
        (false, false) => Some(format!("{prompt} {mentions}")),
    }
}

/// Where the picker's keyboard cursor lands, wrapping at both ends.
///
/// `None` for `current` means the cursor has not moved yet, so `down` opens on
/// the first row and `up` on the last. `None` in the result means the key does
/// not navigate.
pub(super) fn next_picker_highlight(
    current: Option<usize>,
    len: usize,
    key: &str,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match key {
        "down" => Some(current.map_or(0, |index| (index + 1) % len)),
        "up" => Some(current.map_or(len - 1, |index| (index + len - 1) % len)),
        _ => None,
    }
}

/// The sidebar tabs the picker can land on, in rail order: favorites first,
/// then every installed provider a new session may use.
///
/// Shared by the rail's click gating and by `tab`'s cycle handler so the two
/// agree on which tabs are usable. A locked session keeps its own provider
/// usable even if it was switched off afterwards — disabling is for new work —
/// while every other provider drops out for the lock's duration.
pub(super) fn catalog_model_ids(
    catalog: Option<&waku_client::ModelCatalog>,
    manual: &[String],
    default_model: &str,
) -> Vec<String> {
    match catalog {
        Some(catalog) => catalog
            .models
            .iter()
            .map(|entry| entry.id.clone())
            .collect(),
        None => {
            if manual.is_empty() {
                vec![default_model.to_owned()]
            } else {
                manual.to_vec()
            }
        }
    }
}

pub(super) fn catalog_entry_selectable(entry: Option<&waku_client::ModelCatalogEntry>) -> bool {
    entry.is_none_or(|entry| entry.supported)
}

pub(super) fn visible_picker_models(
    providers: &[ExternalProvider],
    catalogs: &HashMap<ProviderId, waku_client::ModelCatalog>,
    favorites: &[FavoriteModel],
    locked_provider: Option<&ProviderId>,
    selected_tab: &ModelPickerTab,
    normalized_query: &str,
) -> Vec<(ProviderId, ProviderModel)> {
    let searching = !normalized_query.is_empty();
    let mut models = providers
        .iter()
        .filter(|provider| locked_provider.is_none_or(|locked| locked == &provider.id))
        .flat_map(|provider| {
            catalog_model_ids(
                catalogs.get(&provider.id),
                &provider.models,
                &provider.default_model,
            )
            .into_iter()
            .map(move |name| (provider, name))
        })
        .filter_map(|(provider, model_name)| {
            let model = ProviderModel::new(model_name.clone(), model_name).default();
            let searchable =
                format!("{} {} {}", provider.name, provider.id, model.id).to_ascii_lowercase();
            if searching
                && !normalized_query
                    .split_whitespace()
                    .all(|token| searchable.contains(token))
            {
                return None;
            }
            if !searching {
                match selected_tab {
                    ModelPickerTab::Favorites
                        if !favorites.iter().any(|favorite| {
                            favorite.provider == provider.id && favorite.model == model.id
                        }) =>
                    {
                        return None;
                    }
                    ModelPickerTab::Provider(selected) if selected != &provider.id => return None,
                    _ => {}
                }
            }
            Some((provider.id.clone(), model))
        })
        .collect::<Vec<_>>();
    if !searching && *selected_tab == ModelPickerTab::Favorites {
        models.sort_by_key(|(provider, model)| {
            favorites
                .iter()
                .position(|favorite| favorite.provider == *provider && favorite.model == model.id)
                .unwrap_or(usize::MAX)
        });
    }
    models
}

#[cfg(test)]
mod catalog_picker_behavior_tests {
    use super::{catalog_entry_selectable, catalog_model_ids};
    use waku_client::{
        ApiFormat, CatalogSource, ModelCapabilities, ModelCatalog, ModelCatalogEntry, ProviderId,
        TransportProfile,
    };

    fn catalog(source: CatalogSource, ids: &[&str]) -> ModelCatalog {
        ModelCatalog {
            provider: ProviderId::new("openai"),
            source,
            fetched_at_ms: 0,
            models: ids
                .iter()
                .map(|id| ModelCatalogEntry {
                    id: (*id).into(),
                    name: (*id).into(),
                    provider: ProviderId::new("openai"),
                    api_format: ApiFormat::OpenAiResponses,
                    transport: TransportProfile::Standard,
                    base_url: "https://api.openai.com/v1".into(),
                    context_window: 128_000,
                    max_output_tokens: 16_384,
                    reasoning: false,
                    capabilities: ModelCapabilities::openai_api(ApiFormat::OpenAiResponses),
                    supported: true,
                    unsupported_reason: None,
                })
                .collect(),
        }
    }

    #[test]
    fn live_empty_catalog_is_authoritative_and_hides_manual() {
        let live = catalog(CatalogSource::Live, &[]);
        assert!(catalog_model_ids(Some(&live), &["manual".into()], "default").is_empty());
    }

    #[test]
    fn missing_catalog_falls_back_to_manual_models() {
        assert_eq!(
            catalog_model_ids(None, &["manual".into()], "default"),
            vec!["manual"]
        );
    }

    #[test]
    fn unsupported_catalog_rows_cannot_be_selected() {
        let mut entry = catalog(CatalogSource::Live, &["grok-imagine-1"]).models;
        entry[0].supported = false;
        assert!(!catalog_entry_selectable(entry.first()));
        assert!(catalog_entry_selectable(None));
    }
}
