use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseMoveEvent, ParentElement, SharedString, Styled, Window, div,
    list, px, relative,
};
use uuid::Uuid;
use wakuwaku_protocol::{
    TRAJECTORY_PAGE_DEFAULT, TrajectoryAvailability, TrajectoryLane, TrajectoryResponse,
    TrajectoryStatus,
};

use crate::app::Waku;
use crate::app::trajectory::{
    TRAJECTORY_LEDGER_MIN_WIDTH, TrajectoryLedgerRowKind, TrajectoryLoadingStatus,
    TrajectorySessionState, format_exact_duration, inspector_uses_split,
    inspector_width_after_drag, inspector_width_after_nudge, item_count_label, localized_lane_name,
    localized_status_name, record_count_label, status_marker, step_ledger_selection,
};
use crate::theme::Theme;
use crate::ui::text_field::TextField;
use crate::ui::tooltip::Tooltip;

impl Waku {
    pub(super) fn render_trajectory(
        &mut self,
        width: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(session_id) = self.state.selected_session else {
            return self.render_empty_state(cx).into_any_element();
        };

        self.ensure_trajectory_session_state(session_id, cx);
        self.bind_trajectory_search(session_id, cx);

        let (inspector_open, selected_record_id, inspector_width) = {
            let mut sessions = self.trajectory_sessions.borrow_mut();
            let Some(state) = sessions.get_mut(&session_id) else {
                return div().into_any_element();
            };
            state.set_inspector_width(state.inspector_width, width);
            (
                state.inspector_open && state.selected_record_id.is_some(),
                state.selected_record_id,
                state.inspector_width,
            )
        };

        let list_focus = self.transcript_control_focus("trajectory-ledger", cx);
        let container = div()
            .id("trajectory-main-container")
            .size_full()
            .flex()
            .flex_col()
            .min_h_0()
            .min_w_0()
            .bg(theme.surface)
            .track_focus(&list_focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                this.drag_trajectory_inspector(session_id, width, f32::from(event.position.x), cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.end_trajectory_inspector_drag(session_id, cx);
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if this.trajectory_search.read(cx).is_visually_focused(window) {
                    if event.keystroke.key.as_str() == "escape" {
                        this.clear_trajectory_search(session_id, cx);
                        cx.stop_propagation();
                    }
                    return;
                }
                if this.handle_trajectory_list_key(session_id, event, cx) {
                    cx.stop_propagation();
                }
            }));

        if inspector_open && let Some(rec_id) = selected_record_id {
            if inspector_uses_split(width) {
                let toolbar = self.render_trajectory_toolbar(session_id, cx);
                let timeline = self.render_trajectory_timeline(session_id, window, cx);
                let banner = self.render_trajectory_banners(session_id, cx);
                let ledger = self.render_trajectory_ledger(session_id, cx);
                let inspector = self.render_trajectory_inspector(session_id, rec_id, false, cx);
                let resizer =
                    self.render_trajectory_inspector_resizer(session_id, width, &theme, cx);
                return container
                    .child(toolbar)
                    .child(timeline)
                    .children(banner)
                    .child(
                        div()
                            .id("trajectory-split")
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .flex()
                            .child(
                                ledger
                                    .flex_1()
                                    .min_w(px(TRAJECTORY_LEDGER_MIN_WIDTH))
                                    .min_h_0(),
                            )
                            .child(resizer)
                            .child(inspector.w(px(inspector_width)).h_full().flex_none()),
                    )
                    .into_any_element();
            }
            let inspector = self.render_trajectory_inspector(session_id, rec_id, true, cx);
            return container.child(inspector).into_any_element();
        }

        let toolbar = self.render_trajectory_toolbar(session_id, cx);
        let timeline = self.render_trajectory_timeline(session_id, window, cx);
        let banner = self.render_trajectory_banners(session_id, cx);
        let ledger = self.render_trajectory_ledger(session_id, cx);

        container
            .child(toolbar)
            .child(timeline)
            .children(banner)
            .child(ledger)
            .into_any_element()
    }

    fn render_trajectory_inspector_resizer(
        &self,
        session_id: Uuid,
        available_width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let focus = self.transcript_control_focus("trajectory-inspector-resize", cx);
        div()
            .id("trajectory-inspector-resize")
            .track_focus(&focus)
            .tab_index(0)
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .w(px(8.0))
            .h_full()
            .flex_none()
            .cursor_col_resize()
            .bg(theme.border)
            .hover(|style| style.bg(theme.resize_handle))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.begin_trajectory_inspector_drag(
                        session_id,
                        f32::from(event.position.x),
                        cx,
                    );
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                let steps = match event.keystroke.key.as_str() {
                    "left" => 1,
                    "right" => -1,
                    _ => return,
                };
                this.nudge_trajectory_inspector(session_id, available_width, steps, cx);
                cx.stop_propagation();
            }))
    }

    fn begin_trajectory_inspector_drag(
        &mut self,
        session_id: Uuid,
        start_x: f32,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.trajectory_sessions.borrow_mut().get_mut(&session_id) {
            state.inspector_resize_anchor = Some((start_x, state.inspector_width));
        }
        cx.notify();
    }

    fn drag_trajectory_inspector(
        &mut self,
        session_id: Uuid,
        available_width: f32,
        current_x: f32,
        cx: &mut Context<Self>,
    ) {
        let mut sessions = self.trajectory_sessions.borrow_mut();
        let Some(state) = sessions.get_mut(&session_id) else {
            return;
        };
        let Some((start_x, start_width)) = state.inspector_resize_anchor else {
            return;
        };
        state.inspector_width =
            inspector_width_after_drag(start_width, start_x, current_x, available_width);
        drop(sessions);
        cx.notify();
    }

    fn end_trajectory_inspector_drag(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if let Some(state) = self.trajectory_sessions.borrow_mut().get_mut(&session_id)
            && state.inspector_resize_anchor.take().is_some()
        {
            cx.notify();
        }
    }

    fn nudge_trajectory_inspector(
        &mut self,
        session_id: Uuid,
        available_width: f32,
        steps: i32,
        cx: &mut Context<Self>,
    ) {
        if let Some(state) = self.trajectory_sessions.borrow_mut().get_mut(&session_id) {
            state.inspector_width =
                inspector_width_after_nudge(state.inspector_width, steps, available_width);
        }
        cx.notify();
    }

    fn bind_trajectory_search(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if self.trajectory_search_session.get() == Some(session_id) {
            return;
        }
        let query = self
            .trajectory_sessions
            .borrow()
            .get(&session_id)
            .map(|state| state.search_query.clone())
            .unwrap_or_default();
        self.trajectory_search_session.set(Some(session_id));
        self.trajectory_search
            .update(cx, |input, cx| input.set_content(query, cx));
    }

    fn clear_trajectory_search(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if let Some(state) = self.trajectory_sessions.borrow_mut().get_mut(&session_id) {
            state.set_search_query(String::new());
        }
        self.trajectory_search_session.set(Some(session_id));
        self.trajectory_search
            .update(cx, |input, cx| input.set_content(String::new(), cx));
        cx.notify();
    }

    fn handle_trajectory_list_key(
        &mut self,
        session_id: Uuid,
        event: &KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let mut sessions = self.trajectory_sessions.borrow_mut();
        let Some(state) = sessions.get_mut(&session_id) else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                if state.inspector_open {
                    state.close_inspector();
                    cx.notify();
                    return true;
                }
                if !state.search_query.is_empty() {
                    drop(sessions);
                    self.clear_trajectory_search(session_id, cx);
                    return true;
                }
                false
            }
            "up" | "down" | "home" | "end" => {
                let next =
                    step_ledger_selection(state.selected_row_index, state.ledger_rows.len(), key);
                if let Some(index) = next {
                    state.select_row_at_index(index);
                    cx.notify();
                    return true;
                }
                false
            }
            "enter" | "space" => {
                if let Some(idx) = state.selected_row_index
                    && let Some(row) = state.ledger_rows.get(idx)
                {
                    match &row.kind {
                        TrajectoryLedgerRowKind::TurnDivider { turn_count, .. } => {
                            let turn = *turn_count;
                            state.toggle_turn_fold(turn);
                            cx.notify();
                            return true;
                        }
                        _ => {
                            if let Some(record_id) = row.record_id {
                                state.select_record(record_id);
                                cx.notify();
                                return true;
                            }
                        }
                    }
                }
                false
            }
            "left" => {
                if let Some(idx) = state.selected_row_index
                    && let Some(row) = state.ledger_rows.get(idx)
                    && let TrajectoryLedgerRowKind::TurnDivider {
                        turn_count,
                        collapsed: false,
                        ..
                    } = row.kind
                {
                    state.toggle_turn_fold(turn_count);
                    cx.notify();
                    return true;
                }
                false
            }
            "right" => {
                if let Some(idx) = state.selected_row_index
                    && let Some(row) = state.ledger_rows.get(idx)
                    && let TrajectoryLedgerRowKind::TurnDivider {
                        turn_count,
                        collapsed: true,
                        ..
                    } = row.kind
                {
                    state.toggle_turn_fold(turn_count);
                    cx.notify();
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn retry_trajectory_load(&mut self, session_id: Uuid, cx: &mut Context<Self>) {
        if let Some(state) = self.trajectory_sessions.borrow_mut().get_mut(&session_id) {
            state.loading_status = TrajectoryLoadingStatus::Initial;
            state.error = None;
        }
        cx.notify();
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
                                state.error = Some(tr!("trajectory.unexpected_response"));
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

        let duration_projection = state.duration_projection;
        let show_tool_calls = state.show_tool_calls;
        let any_folded = !state.folded_turns.is_empty();
        drop(sessions);

        let search_box = TextField::new("trajectory-search", self.trajectory_search.clone())
            .icon("icons/search.svg", 13.0)
            .flex_1()
            .min_w(px(140.0))
            .max_w(px(320.0));

        let duration_btn = trajectory_toolbar_button(
            "trajectory-duration-toggle-btn",
            tr!("trajectory.duration_projection"),
            duration_projection,
            &theme,
            self.transcript_control_focus("trajectory-duration-toggle", cx),
            cx,
            move |this, cx| {
                if let Some(state) = this.trajectory_sessions.borrow_mut().get_mut(&session_id) {
                    state.toggle_duration_projection();
                    cx.notify();
                }
            },
        );
        let turns_btn = trajectory_toolbar_button(
            "trajectory-turns-toggle-btn",
            if any_folded {
                tr!("trajectory.unfold_all_turns")
            } else {
                tr!("trajectory.fold_all_turns")
            },
            any_folded,
            &theme,
            self.transcript_control_focus("trajectory-turns-toggle", cx),
            cx,
            move |this, cx| {
                if let Some(state) = this.trajectory_sessions.borrow_mut().get_mut(&session_id) {
                    if any_folded {
                        state.unfold_all_turns();
                    } else {
                        state.fold_all_turns();
                    }
                    cx.notify();
                }
            },
        );
        let calls_btn = trajectory_toolbar_button(
            "trajectory-calls-toggle-btn",
            tr!("trajectory.calls"),
            show_tool_calls,
            &theme,
            self.transcript_control_focus("trajectory-calls-toggle", cx),
            cx,
            move |this, cx| {
                if let Some(state) = this.trajectory_sessions.borrow_mut().get_mut(&session_id) {
                    state.toggle_tool_calls();
                    cx.notify();
                }
            },
        );

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

    fn render_trajectory_timeline(
        &self,
        session_id: Uuid,
        _window: &Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let sessions = self.trajectory_sessions.borrow();
        let Some(state) = sessions.get(&session_id) else {
            return div();
        };

        let selected_record_id = state.selected_record_id;
        let has_any_timing = state.timeline_layout.has_any_timing;
        let has_records = !state.records.is_empty();
        let lanes = [
            (
                localized_lane_name(TrajectoryLane::Input),
                state.timeline_layout.input_spans.clone(),
            ),
            (
                localized_lane_name(TrajectoryLane::Model),
                state.timeline_layout.model_spans.clone(),
            ),
            (
                localized_lane_name(TrajectoryLane::Tools),
                state.timeline_layout.tools_spans.clone(),
            ),
        ];
        drop(sessions);

        let mut root = div()
            .px(px(12.0))
            .py(px(8.0))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface);

        for (name, spans) in lanes {
            let mut track = div()
                .flex_1()
                .h(px(16.0))
                .rounded(px(3.0))
                .bg(theme.raised)
                .relative()
                .overflow_hidden();
            for span in &spans {
                let rec_id = span.record_id;
                let is_selected = selected_record_id == Some(rec_id);
                let left_pct = span.start_pct * 100.0;
                let width_pct = (span.width_pct * 100.0).max(1.5);
                let bg_color = match span.lane {
                    TrajectoryLane::Input => theme.text_secondary.opacity(0.4),
                    TrajectoryLane::Model => theme.accent.opacity(0.8),
                    TrajectoryLane::Tools => theme.accent.opacity(0.5),
                };
                let tooltip = format!(
                    "{} · {} · {} · {}",
                    span.title,
                    localized_status_name(span.status),
                    if span.has_timing {
                        span.duration_text.clone()
                    } else {
                        tr!("trajectory.no_timing_data")
                    },
                    if is_selected {
                        tr!("trajectory.selected")
                    } else {
                        String::new()
                    }
                );
                let focus = self.transcript_control_focus(format!("trajectory-span-{rec_id}"), cx);
                let bar = div()
                    .id(SharedString::from(format!("timeline-span-{rec_id}")))
                    .track_focus(&focus)
                    .tab_index(0)
                    .focus_visible(|style| style.border_1().border_color(theme.text))
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
                    .hover(|style| style.opacity(0.9))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        if let Some(state) =
                            this.trajectory_sessions.borrow_mut().get_mut(&session_id)
                        {
                            state.select_record(rec_id);
                            cx.notify();
                        }
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            if let Some(state) =
                                this.trajectory_sessions.borrow_mut().get_mut(&session_id)
                            {
                                state.select_record(rec_id);
                                cx.notify();
                            }
                            cx.stop_propagation();
                        }
                    }));
                track = track.child(bar);
            }
            root = root.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(48.0))
                            .text_size(px(10.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_tertiary)
                            .child(name),
                    )
                    .child(track),
            );
        }

        root.when(!has_any_timing && has_records, |element| {
            element.child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme.text_tertiary)
                    .text_center()
                    .child(tr!("trajectory.no_timing_data")),
            )
        })
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
            drop(sessions);
            let retry_focus = self.transcript_control_focus("trajectory-retry", cx);
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
                            .track_focus(&retry_focus)
                            .tab_index(0)
                            .focus_visible(|style| style.border_1().border_color(theme.text))
                            .px(px(8.0))
                            .py(px(3.0))
                            .rounded(px(4.0))
                            .bg(theme.accent)
                            .text_color(theme.on_inverse)
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.retry_trajectory_load(session_id, cx);
                            }))
                            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                    this.retry_trajectory_load(session_id, cx);
                                    cx.stop_propagation();
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
            let older_cursor = state.older_cursor;
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
        let focus = self.transcript_control_focus(format!("trajectory-row-{}", row.key), cx);

        render_prepared_ledger_row(
            PreparedLedgerRow {
                session_id,
                row: &row,
                row_ix,
                is_selected,
                duration_projection,
                theme: &theme,
                focus,
            },
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
                        if let Some(anchor) = anchor_id
                            && let Some(new_idx) = state
                                .ledger_rows
                                .iter()
                                .position(|r| r.record_id == Some(anchor))
                        {
                            state.list_state.scroll_to_reveal_item(new_idx);
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

struct PreparedLedgerRow<'a> {
    session_id: Uuid,
    row: &'a crate::app::trajectory::TrajectoryLedgerRow,
    row_ix: usize,
    is_selected: bool,
    duration_projection: bool,
    theme: &'a Theme,
    focus: FocusHandle,
}

fn render_prepared_ledger_row(
    prepared: PreparedLedgerRow<'_>,
    cx: &mut Context<Waku>,
) -> impl IntoElement {
    let PreparedLedgerRow {
        session_id,
        row,
        row_ix,
        is_selected,
        duration_projection,
        theme,
        focus,
    } = prepared;
    let mut item = div()
        .id(SharedString::from(format!("trajectory-row-{}", row.key)))
        .track_focus(&focus)
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(theme.accent))
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
        .hover(|style| style.bg(theme.accent.opacity(0.04)))
        .cursor_pointer()
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if let Some(state) = this.trajectory_sessions.borrow_mut().get_mut(&session_id) {
                state.select_row_at_index(row_ix);
            }
            if this.handle_trajectory_list_key(session_id, event, cx) {
                cx.stop_propagation();
            }
        }));

    if is_selected {
        item = item.child(
            div()
                .text_size(px(10.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(format!("› {}", tr!("trajectory.selected"))),
        );
    }

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
                    if let Some(state) = this.trajectory_sessions.borrow_mut().get_mut(&session_id)
                    {
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
                                .text_size(px(10.0))
                                .text_color(theme.text_tertiary)
                                .child(if is_collapsed {
                                    format!("▶ {}", tr!("trajectory.collapsed"))
                                } else {
                                    format!("▼ {}", tr!("trajectory.expanded"))
                                }),
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
                                .child(record_count_label(rec_cnt)),
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
                                    .child(item_count_label(children_cnt)),
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
                                .child(tr!("trajectory.assistant_label")),
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
                                .child(tr!("trajectory.tool_label")),
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
                                .child(format!(
                                    "{} {}",
                                    status_marker(status),
                                    localized_status_name(status)
                                )),
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

fn trajectory_toolbar_button<T: Fn(&mut Waku, &mut Context<Waku>) + 'static>(
    id: &'static str,
    label: String,
    pressed: bool,
    theme: &Theme,
    focus: FocusHandle,
    cx: &mut Context<Waku>,
    on_activate: T,
) -> impl IntoElement + use<T> {
    let on_click = std::rc::Rc::new(on_activate);
    let on_key = on_click.clone();
    div()
        .id(id)
        .track_focus(&focus)
        .tab_index(0)
        .focus_visible(|style| style.border_1().border_color(theme.accent))
        .px(px(8.0))
        .py(px(4.0))
        .rounded(px(4.0))
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(if pressed {
            theme.accent
        } else {
            theme.text_secondary
        })
        .bg(if pressed {
            theme.accent.opacity(0.12)
        } else {
            theme.raised
        })
        .border_1()
        .border_color(if pressed {
            theme.accent.opacity(0.3)
        } else {
            theme.border
        })
        .cursor_pointer()
        .hover(|style| style.bg(theme.accent.opacity(0.08)))
        .on_click({
            let on_click = on_click.clone();
            cx.listener(move |this, _: &ClickEvent, _, cx| on_click(this, cx))
        })
        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                on_key(this, cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .when(pressed, |element| element.child("✓"))
                .child(label),
        )
}
