use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, SharedString, Styled, Window, div, list, px, relative,
};
use uuid::Uuid;
use wakuwaku_protocol::{
    TRAJECTORY_PAGE_DEFAULT, TrajectoryAvailability, TrajectoryLane, TrajectoryResponse,
    TrajectoryStatus,
};

use crate::app::Waku;
use crate::app::trajectory::{
    TrajectoryLedgerRowKind, TrajectoryLoadingStatus, TrajectorySessionState,
    format_exact_duration, status_display_name,
};
use crate::theme::Theme;
use crate::ui::icon;

impl Waku {
    pub(super) fn render_trajectory(
        &mut self,
        _width: f32,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session_id) = self.state.selected_session else {
            return self.render_empty_state(cx).into_any_element();
        };

        self.ensure_trajectory_session_state(session_id, cx);

        let (inspector_open, selected_record_id) = {
            let sessions = self.trajectory_sessions.borrow();
            let Some(state) = sessions.get(&session_id) else {
                return div().into_any_element();
            };
            (
                state.inspector_open && state.selected_record_id.is_some(),
                state.selected_record_id,
            )
        };

        let container = div()
            .id("trajectory-main-container")
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .min_w_0()
            .bg(theme.surface)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let mut sessions = this.trajectory_sessions.borrow_mut();
                let Some(state) = sessions.get_mut(&session_id) else {
                    return;
                };

                match event.keystroke.key.as_str() {
                    "escape" => {
                        if state.inspector_open {
                            state.close_inspector();
                            cx.notify();
                        } else if !state.search_query.is_empty() {
                            state.set_search_query(String::new());
                            cx.notify();
                        }
                    }
                    "up" => {
                        let current = state.selected_row_index.unwrap_or(0);
                        if current > 0 {
                            state.select_row_at_index(current - 1);
                            cx.notify();
                        }
                    }
                    "down" => {
                        let current = state.selected_row_index.unwrap_or(0);
                        if current + 1 < state.ledger_rows.len() {
                            state.select_row_at_index(current + 1);
                            cx.notify();
                        }
                    }
                    "home" => {
                        if !state.ledger_rows.is_empty() {
                            state.select_row_at_index(0);
                            cx.notify();
                        }
                    }
                    "end" => {
                        let count = state.ledger_rows.len();
                        if count > 0 {
                            state.select_row_at_index(count - 1);
                            cx.notify();
                        }
                    }
                    "enter" | "space" => {
                        if let Some(idx) = state.selected_row_index {
                            if let Some(row) = state.ledger_rows.get(idx) {
                                match &row.kind {
                                    TrajectoryLedgerRowKind::TurnDivider { turn_count, .. } => {
                                        let turn = *turn_count;
                                        state.toggle_turn_fold(turn);
                                        cx.notify();
                                    }
                                    _ => {
                                        if let Some(record_id) = row.record_id {
                                            state.select_record(record_id);
                                            cx.notify();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "left" => {
                        if let Some(idx) = state.selected_row_index {
                            if let Some(row) = state.ledger_rows.get(idx) {
                                if let TrajectoryLedgerRowKind::TurnDivider {
                                    turn_count,
                                    collapsed: false,
                                    ..
                                } = row.kind
                                {
                                    state.toggle_turn_fold(turn_count);
                                    cx.notify();
                                }
                            }
                        }
                    }
                    "right" => {
                        if let Some(idx) = state.selected_row_index {
                            if let Some(row) = state.ledger_rows.get(idx) {
                                if let TrajectoryLedgerRowKind::TurnDivider {
                                    turn_count,
                                    collapsed: true,
                                    ..
                                } = row.kind
                                {
                                    state.toggle_turn_fold(turn_count);
                                    cx.notify();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }));

        if inspector_open {
            if let Some(rec_id) = selected_record_id {
                let inspector = self.render_trajectory_inspector(session_id, rec_id, true, cx);
                return container.child(inspector).into_any_element();
            }
        }

        let toolbar = self.render_trajectory_toolbar(session_id, cx);
        let timeline = self.render_trajectory_timeline(session_id, cx);
        let banner = self.render_trajectory_banners(session_id, cx);
        let ledger = self.render_trajectory_ledger(session_id, cx);

        container
            .child(toolbar)
            .child(timeline)
            .children(banner)
            .child(ledger)
            .into_any_element()
    }

    pub(super) fn ensure_trajectory_session_state(
        &mut self,
        session_id: Uuid,
        cx: &mut Context<Self>,
    ) {
        let mut sessions = self.trajectory_sessions.borrow_mut();
        let state = sessions
            .entry(session_id)
            .or_insert_with(|| TrajectorySessionState::new(session_id));

        if state.loading_status == TrajectoryLoadingStatus::Initial {
            state.loading_status = TrajectoryLoadingStatus::Loading;
            drop(sessions);

            let daemon_client = self.daemon.client();
            cx.spawn(async move |waku, cx| {
                let trajectory_client = wakuwaku_client::TrajectoryClient::new(daemon_client);
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        trajectory_client.page(
                            session_id,
                            None,
                            Some(TRAJECTORY_PAGE_DEFAULT),
                            None,
                        )
                    })
                    .await;

                let _ = waku.update(cx, |waku, cx| {
                    let mut sessions = waku.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        match result {
                            Ok(TrajectoryResponse::Page {
                                availability,
                                generation,
                                revision,
                                rows,
                                older,
                                newer,
                                has_older,
                                has_newer,
                            }) => {
                                state.set_page_response(
                                    availability,
                                    generation,
                                    revision,
                                    rows,
                                    older,
                                    newer,
                                    has_older,
                                    has_newer,
                                );
                            }
                            Err(err) => {
                                state.loading_status = TrajectoryLoadingStatus::Error;
                                state.error = Some(err.to_string());
                            }
                            _ => {
                                state.loading_status = TrajectoryLoadingStatus::Error;
                                state.error = Some("Unexpected response".into());
                            }
                        }
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    fn render_trajectory_toolbar(&self, session_id: Uuid, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let sessions = self.trajectory_sessions.borrow();
        let Some(state) = sessions.get(&session_id) else {
            return div();
        };

        let search_query = state.search_query.clone();
        let duration_projection = state.duration_projection;
        let show_tool_calls = state.show_tool_calls;
        let any_folded = !state.folded_turns.is_empty();

        // Search Input field
        let search_box = div()
            .flex_1()
            .min_w(px(140.0))
            .max_w(px(320.0))
            .h(px(28.0))
            .px(px(8.0))
            .rounded(px(5.0))
            .bg(theme.raised)
            .border_1()
            .border_color(theme.border)
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(icon("icons/search.svg", 13.0, theme.text_tertiary))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.0))
                    .text_color(if search_query.is_empty() {
                        theme.text_tertiary
                    } else {
                        theme.text
                    })
                    .child(if search_query.is_empty() {
                        tr!("trajectory.search_placeholder")
                    } else {
                        search_query.clone()
                    }),
            )
            .when(!search_query.is_empty(), |element| {
                element.child(
                    div()
                        .id("trajectory-clear-search-btn")
                        .px(px(4.0))
                        .text_size(px(11.0))
                        .text_color(theme.text_tertiary)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            let mut sessions = this.trajectory_sessions.borrow_mut();
                            if let Some(state) = sessions.get_mut(&session_id) {
                                state.set_search_query(String::new());
                                cx.notify();
                            }
                        }))
                        .child("×"),
                )
            });

        // Duration Projection toggle button
        let duration_btn = div()
            .id("trajectory-duration-toggle-btn")
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if duration_projection {
                theme.accent
            } else {
                theme.text_secondary
            })
            .bg(if duration_projection {
                theme.accent.opacity(0.12)
            } else {
                theme.raised
            })
            .border_1()
            .border_color(if duration_projection {
                theme.accent.opacity(0.3)
            } else {
                theme.border
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let mut sessions = this.trajectory_sessions.borrow_mut();
                if let Some(state) = sessions.get_mut(&session_id) {
                    state.toggle_duration_projection();
                    cx.notify();
                }
            }))
            .child(tr!("trajectory.duration_projection"));

        // Turns all fold/unfold button
        let turns_btn = div()
            .id("trajectory-turns-toggle-btn")
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text_secondary)
            .bg(theme.raised)
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let mut sessions = this.trajectory_sessions.borrow_mut();
                if let Some(state) = sessions.get_mut(&session_id) {
                    if any_folded {
                        state.unfold_all_turns();
                    } else {
                        state.fold_all_turns();
                    }
                    cx.notify();
                }
            }))
            .child(if any_folded {
                tr!("trajectory.unfold_all_turns")
            } else {
                tr!("trajectory.fold_all_turns")
            });

        // Calls tool-row visibility toggle button
        let calls_btn = div()
            .id("trajectory-calls-toggle-btn")
            .px(px(8.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if show_tool_calls {
                theme.accent
            } else {
                theme.text_secondary
            })
            .bg(if show_tool_calls {
                theme.accent.opacity(0.12)
            } else {
                theme.raised
            })
            .border_1()
            .border_color(if show_tool_calls {
                theme.accent.opacity(0.3)
            } else {
                theme.border
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let mut sessions = this.trajectory_sessions.borrow_mut();
                if let Some(state) = sessions.get_mut(&session_id) {
                    state.toggle_tool_calls();
                    cx.notify();
                }
            }))
            .child(tr!("trajectory.calls"));

        div()
            .h(px(42.0))
            .px(px(12.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(search_box)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(duration_btn)
                    .child(turns_btn)
                    .child(calls_btn),
            )
    }

    fn render_trajectory_timeline(&self, session_id: Uuid, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let sessions = self.trajectory_sessions.borrow();
        let Some(state) = sessions.get(&session_id) else {
            return div();
        };

        let layout = &state.timeline_layout;
        let selected_record_id = state.selected_record_id;

        let render_lane = |name: &'static str, spans: &[crate::app::trajectory::TimelineSpan]| {
            let mut track = div()
                .flex_1()
                .h(px(16.0))
                .rounded(px(3.0))
                .bg(theme.raised)
                .relative()
                .overflow_hidden();

            for span in spans {
                let rec_id = span.record_id;
                let is_selected = selected_record_id == Some(rec_id);
                let left_pct = span.start_pct * 100.0;
                let width_pct = (span.width_pct * 100.0).max(1.5);

                let bg_color = match span.lane {
                    TrajectoryLane::Input => theme.text_secondary.opacity(0.4),
                    TrajectoryLane::Model => theme.accent.opacity(0.8),
                    TrajectoryLane::Tools => theme.accent.opacity(0.5),
                };

                let bar = div()
                    .id(SharedString::from(format!(
                        "timeline-span-{}",
                        span.record_id
                    )))
                    .absolute()
                    .left(relative(left_pct / 100.0))
                    .w(relative(width_pct / 100.0))
                    .h_full()
                    .rounded(px(2.0))
                    .bg(if is_selected { theme.accent } else { bg_color })
                    .border_1()
                    .border_color(if is_selected {
                        theme.text
                    } else {
                        gpui::hsla(0.0, 0.0, 0.0, 0.0)
                    })
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let mut sessions = this.trajectory_sessions.borrow_mut();
                        if let Some(state) = sessions.get_mut(&session_id) {
                            state.select_record(rec_id);
                            cx.notify();
                        }
                    }));

                track = track.child(bar);
            }

            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .w(px(40.0))
                        .text_size(px(10.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_tertiary)
                        .child(name),
                )
                .child(track)
        };

        div()
            .px(px(12.0))
            .py(px(8.0))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(render_lane("Input", &layout.input_spans))
            .child(render_lane("Model", &layout.model_spans))
            .child(render_lane("Tools", &layout.tools_spans))
            .when(
                !layout.has_any_timing && !state.records.is_empty(),
                |element| {
                    element.child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.text_tertiary)
                            .text_center()
                            .child(tr!("trajectory.no_timing_data")),
                    )
                },
            )
    }

    fn render_trajectory_banners(&self, session_id: Uuid, cx: &mut Context<Self>) -> Option<Div> {
        let theme = Theme::current(cx);
        let sessions = self.trajectory_sessions.borrow();
        let state = sessions.get(&session_id)?;

        if state.loading_status == TrajectoryLoadingStatus::Loading && state.records.is_empty() {
            return Some(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .bg(theme.accent.opacity(0.08))
                    .border_b_1()
                    .border_color(theme.accent.opacity(0.2))
                    .text_size(px(12.0))
                    .text_color(theme.accent)
                    .child(tr!("trajectory.loading")),
            );
        }

        if state.availability == TrajectoryAvailability::Legacy {
            return Some(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .bg(theme.raised)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("trajectory.legacy_partial_banner")),
            );
        }

        if state.availability == TrajectoryAvailability::LegacyPartialMissingSnapshot {
            return Some(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .bg(theme.raised)
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(px(12.0))
                    .text_color(theme.text_secondary)
                    .child(tr!("trajectory.missing_snapshot_banner")),
            );
        }

        if state.availability == TrajectoryAvailability::Error || state.error.is_some() {
            return Some(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .bg(theme.accent.opacity(0.1))
                    .border_b_1()
                    .border_color(theme.accent.opacity(0.3))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.accent)
                            .child(tr!("trajectory.error_banner")),
                    )
                    .child(
                        div()
                            .id("trajectory-retry-btn")
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .bg(theme.accent)
                            .text_color(theme.text)
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let mut sessions = this.trajectory_sessions.borrow_mut();
                                if let Some(state) = sessions.get_mut(&session_id) {
                                    state.loading_status = TrajectoryLoadingStatus::Initial;
                                    cx.notify();
                                }
                            }))
                            .child(tr!("trajectory.retry")),
                    ),
            );
        }

        None
    }

    fn render_trajectory_ledger(&mut self, session_id: Uuid, cx: &mut Context<Self>) -> Div {
        let theme = Theme::current(cx);
        let sessions = self.trajectory_sessions.borrow();
        let Some(state) = sessions.get(&session_id) else {
            return div();
        };

        if state.records.is_empty() && state.loading_status == TrajectoryLoadingStatus::Ready {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(tr!("trajectory.empty")),
                )
                .child(
                    div()
                        .mt(px(6.0))
                        .text_size(px(12.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("trajectory.empty_description")),
                );
        }

        let list_state = state.list_state.clone();

        // Check if older page fetch is needed (within 48px / top of list)
        if state.has_older
            && !state.loading_older
            && state.list_state.logical_scroll_top().item_ix <= 1
        {
            let older_cursor = state.older_cursor.clone();
            drop(sessions);
            self.load_older_trajectory_records(session_id, older_cursor, cx);
        }

        let entity = cx.weak_entity();
        div().flex_1().min_h_0().relative().child(
            list(list_state.clone(), move |row_ix, _window, cx| {
                entity
                    .upgrade()
                    .map(|entity| {
                        entity.update(cx, |this, cx| {
                            this.render_trajectory_ledger_row(session_id, row_ix, cx)
                        })
                    })
                    .unwrap_or_else(|| div().into_any_element())
            })
            .size_full(),
        )
    }

    fn render_trajectory_ledger_row(
        &mut self,
        session_id: Uuid,
        row_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let sessions = self.trajectory_sessions.borrow();
        let Some(state) = sessions.get(&session_id) else {
            return div().into_any_element();
        };
        let Some(row) = state.ledger_rows.get(row_ix).cloned() else {
            return div().into_any_element();
        };
        let is_selected = state.selected_row_index == Some(row_ix);
        let duration_projection = state.duration_projection;
        drop(sessions);

        render_prepared_ledger_row(
            session_id,
            &row,
            row_ix,
            is_selected,
            duration_projection,
            &theme,
            cx,
        )
        .into_any_element()
    }

    fn load_older_trajectory_records(
        &mut self,
        session_id: Uuid,
        before: Option<wakuwaku_protocol::TrajectoryCursor>,
        cx: &mut Context<Self>,
    ) {
        let mut sessions = self.trajectory_sessions.borrow_mut();
        let Some(state) = sessions.get_mut(&session_id) else {
            return;
        };
        if state.loading_older {
            return;
        }
        state.loading_older = true;
        drop(sessions);

        let daemon_client = self.daemon.client();
        cx.spawn(async move |waku, cx| {
            let trajectory_client = wakuwaku_client::TrajectoryClient::new(daemon_client);
            let result = cx
                .background_executor()
                .spawn(async move {
                    trajectory_client.page(session_id, before, Some(TRAJECTORY_PAGE_DEFAULT), None)
                })
                .await;

            let _ = waku.update(cx, |waku, cx| {
                let mut sessions = waku.trajectory_sessions.borrow_mut();
                if let Some(state) = sessions.get_mut(&session_id) {
                    if let Ok(TrajectoryResponse::Page {
                        rows,
                        older,
                        has_older,
                        ..
                    }) = result
                    {
                        let anchor_id = state.prepend_older_page(rows, older, has_older);
                        if let Some(anchor) = anchor_id {
                            if let Some(new_idx) = state
                                .ledger_rows
                                .iter()
                                .position(|r| r.record_id == Some(anchor))
                            {
                                state.list_state.scroll_to_reveal_item(new_idx);
                            }
                        }
                    } else {
                        state.loading_older = false;
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

fn render_prepared_ledger_row(
    session_id: Uuid,
    row: &crate::app::trajectory::TrajectoryLedgerRow,
    _row_ix: usize,
    is_selected: bool,
    duration_projection: bool,
    theme: &Theme,
    cx: &mut Context<Waku>,
) -> impl IntoElement {
    let mut item = div()
        .id(SharedString::from(format!("trajectory-row-{}", row.key)))
        .w_full()
        .px(px(12.0))
        .py(px(6.0))
        .border_l_2()
        .border_color(if is_selected {
            theme.accent
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.0)
        })
        .bg(if is_selected {
            theme.accent.opacity(0.08)
        } else {
            gpui::hsla(0.0, 0.0, 0.0, 0.0)
        })
        .hover(|s| s.bg(theme.accent.opacity(0.04)))
        .cursor_pointer();

    let _record_id_opt = row.record_id;

    match &row.kind {
        TrajectoryLedgerRowKind::TurnDivider {
            turn_count,
            collapsed,
            record_count,
            total_duration_ms,
        } => {
            let turn = *turn_count;
            let is_collapsed = *collapsed;
            let rec_cnt = *record_count;
            let dur_ms = *total_duration_ms;

            item = item
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.toggle_turn_fold(turn);
                        cx.notify();
                    }
                }))
                .bg(theme.raised)
                .border_y_1()
                .border_color(theme.border)
                .flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .w(px(14.0))
                                .text_size(px(10.0))
                                .text_color(theme.text_tertiary)
                                .child(if is_collapsed { "▶" } else { "▼" }),
                        )
                        .child(
                            div()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.text)
                                .child(tr!("trajectory.turn_divider", turn = turn)),
                        )
                        .child(
                            div()
                                .px(px(5.0))
                                .py(px(1.0))
                                .rounded(px(3.0))
                                .bg(theme.border)
                                .text_size(px(10.5))
                                .text_color(theme.text_secondary)
                                .child(format!("{rec_cnt} records")),
                        ),
                )
                .when(duration_projection && dur_ms.is_some(), |element| {
                    element.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .child(format_exact_duration(dur_ms)),
                    )
                });
        }
        TrajectoryLedgerRowKind::StepRequest {
            record,
            children_count,
        } => {
            let rec_id = record.record_id;
            let step = record.step;
            let dur = record.duration_ms;
            let title = record.title.clone();
            let preview = record.preview.clone();
            let children_cnt = *children_count;

            item = item
                .pl(px(12.0 + (row.depth as f32 * 14.0)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.select_record(rec_id);
                        cx.notify();
                    }
                }))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .min_w_0()
                        .child(
                            div()
                                .px(px(6.0))
                                .py(px(2.0))
                                .rounded(px(4.0))
                                .bg(theme.accent.opacity(0.12))
                                .text_size(px(11.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.accent)
                                .child(tr!("trajectory.step", step = step)),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(if !title.is_empty() { title } else { preview }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .when(children_cnt > 0, |element| {
                            element.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_tertiary)
                                    .child(format!("{children_cnt} items")),
                            )
                        })
                        .when(duration_projection, |element| {
                            element.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_tertiary)
                                    .child(format_exact_duration(dur)),
                            )
                        }),
                );
        }
        TrajectoryLedgerRowKind::Assistant { record } => {
            let rec_id = record.record_id;
            let preview = record.preview.clone();
            let dur = record.duration_ms;

            item = item
                .pl(px(12.0 + (row.depth as f32 * 14.0)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.select_record(rec_id);
                        cx.notify();
                    }
                }))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.accent)
                                .child("Assistant:"),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(theme.text_secondary)
                                .child(preview),
                        ),
                )
                .when(duration_projection, |element| {
                    element.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .child(format_exact_duration(dur)),
                    )
                });
        }
        TrajectoryLedgerRowKind::Tool { record } => {
            let rec_id = record.record_id;
            let title = record.title.clone();
            let preview = record.preview.clone();
            let status = record.status;
            let dur = record.duration_ms;

            item = item
                .pl(px(12.0 + (row.depth as f32 * 14.0)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.select_record(rec_id);
                        cx.notify();
                    }
                }))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .child(
                            div()
                                .px(px(4.0))
                                .py(px(1.0))
                                .rounded(px(3.0))
                                .bg(theme.border)
                                .text_size(px(10.5))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_secondary)
                                .child("Tool"),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(if !title.is_empty() { title } else { preview }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(match status {
                                    TrajectoryStatus::Completed => theme.text_secondary,
                                    TrajectoryStatus::Failed => theme.accent,
                                    _ => theme.text_tertiary,
                                })
                                .child(status_display_name(status)),
                        )
                        .when(duration_projection, |element| {
                            element.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_tertiary)
                                    .child(format_exact_duration(dur)),
                            )
                        }),
                );
        }
        TrajectoryLedgerRowKind::Context { record } => {
            let rec_id = record.record_id;
            let title = record.title.clone();
            let preview = record.preview.clone();

            item = item
                .pl(px(12.0 + (row.depth as f32 * 14.0)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.select_record(rec_id);
                        cx.notify();
                    }
                }))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text_tertiary)
                                .child(tr!("trajectory.context_steering")),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(theme.text_secondary)
                                .child(if !title.is_empty() { title } else { preview }),
                        ),
                );
        }
        TrajectoryLedgerRowKind::System { record } => {
            let rec_id = record.record_id;
            let preview = record.preview.clone();

            item = item
                .pl(px(12.0 + (row.depth as f32 * 14.0)))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.select_record(rec_id);
                        cx.notify();
                    }
                }))
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .min_w_0()
                        .child(
                            div()
                                .text_size(px(11.0))
                                .font_weight(FontWeight::BOLD)
                                .text_color(theme.text_tertiary)
                                .child(tr!("trajectory.system_prompt")),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(theme.text_secondary)
                                .child(preview),
                        ),
                );
        }
        TrajectoryLedgerRowKind::ToolOnlyPlaceholder {
            parent_record_id,
            step: _,
            turn_count: _,
            duration_ms,
        } => {
            let p_id = *parent_record_id;
            let dur = *duration_ms;

            item = item
                .pl(px(12.0 + (row.depth as f32 * 14.0)))
                .when_some(p_id, |el, pid| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let mut sessions = this.trajectory_sessions.borrow_mut();
                        if let Some(state) = sessions.get_mut(&session_id) {
                            state.select_record(pid);
                            cx.notify();
                        }
                    }))
                })
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .text_size(px(12.0))
                        .italic()
                        .text_color(theme.text_tertiary)
                        .child(tr!("trajectory.tool_placeholder")),
                )
                .when(duration_projection, |element| {
                    element.child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme.text_tertiary)
                            .child(format_exact_duration(dur)),
                    )
                });
        }
    }

    item
}
