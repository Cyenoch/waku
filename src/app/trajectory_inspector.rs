use gpui::prelude::*;
use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, SharedString, Styled, div, px,
};
use serde_json::Value;
use uuid::Uuid;
use wakuwaku_protocol::{
    TRAJECTORY_DETAIL_WINDOW_BYTES, TrajectoryDetailContent, TrajectoryDetailSection,
    TrajectoryRowSummary,
};

use super::*;
use crate::app::Waku;
use crate::app::trajectory::{
    JsonValueType, flatten_json_tree, format_exact_duration, kind_display_name, status_display_name,
};
use crate::query::Query;
use crate::theme::Theme;

impl Waku {
    pub(super) fn render_trajectory_inspector(
        &mut self,
        session_id: Uuid,
        record_id: Uuid,
        _is_overlay: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = Theme::current(cx);
        let mut trajectory_sessions = self.trajectory_sessions.borrow_mut();
        let Some(state) = trajectory_sessions.get_mut(&session_id) else {
            return div();
        };

        let Some(record) = state
            .records
            .iter()
            .find(|r| r.record_id == record_id)
            .cloned()
        else {
            return div();
        };

        let selected_section = state.selected_section;
        let allowed_sections = state.allowed_sections_for_record(record_id);

        let mut header_tabs = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .overflow_x_hidden()
            .flex_1()
            .min_w_0();

        for section in allowed_sections {
            let is_selected = section == selected_section;
            let label = localized_section_name(section);
            let section_to_select = section;
            header_tabs = header_tabs.child(
                div()
                    .id(SharedString::from(format!("inspector-tab-{section:?}")))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .text_size(px(12.0))
                    .font_weight(if is_selected {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if is_selected {
                        theme.accent
                    } else {
                        theme.text_secondary
                    })
                    .bg(if is_selected {
                        theme.accent.opacity(0.12)
                    } else {
                        gpui::hsla(0.0, 0.0, 0.0, 0.0)
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.accent.opacity(0.08)))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let mut sessions = this.trajectory_sessions.borrow_mut();
                        if let Some(state) = sessions.get_mut(&session_id) {
                            state.selected_section = section_to_select;
                            cx.notify();
                        }
                    }))
                    .child(label),
            );
        }

        let back_button = div()
            .id("inspector-back-btn")
            .px(px(6.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_size(px(12.0))
            .text_color(theme.text_secondary)
            .hover(|s| s.bg(theme.raised).text_color(theme.text))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let mut sessions = this.trajectory_sessions.borrow_mut();
                if let Some(state) = sessions.get_mut(&session_id) {
                    state.close_inspector();
                    cx.notify();
                }
            }))
            .child(icon("icons/arrow-left.svg", 13.0, theme.text_secondary))
            .child(tr!("trajectory.back"));

        let close_button = div()
            .id("inspector-close-btn")
            .p(px(4.0))
            .rounded(px(4.0))
            .text_color(theme.text_tertiary)
            .hover(|s| s.bg(theme.raised).text_color(theme.text))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                let mut sessions = this.trajectory_sessions.borrow_mut();
                if let Some(state) = sessions.get_mut(&session_id) {
                    state.close_inspector();
                    cx.notify();
                }
            }))
            .child(icon("icons/x.svg", 13.0, theme.text_tertiary));

        let header = div()
            .h(px(40.0))
            .px(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(6.0))
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .child(back_button)
            .child(header_tabs)
            .child(close_button);

        // Fetch / cache detail content
        let cursor = *state
            .detail_cursors
            .get(&(record_id, selected_section))
            .unwrap_or(&0);
        let query_key = (record_id, selected_section, cursor);
        let detail_query = state.detail_cache.read(&query_key);

        let content_element: AnyElement = match detail_query {
            Query::Ready(content) => {
                let content_clone = (*content).clone();
                drop(trajectory_sessions);
                self.render_inspector_content(
                    session_id,
                    &record,
                    selected_section,
                    &content_clone,
                    theme,
                    cx,
                )
            }
            Query::Missing(token) => {
                drop(trajectory_sessions);
                let daemon_client = self.daemon.client();
                cx.spawn(async move |waku, cx| {
                    let trajectory_client = wakuwaku_client::TrajectoryClient::new(daemon_client);
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            trajectory_client.detail(
                                session_id,
                                record_id,
                                selected_section,
                                Some(cursor),
                                Some(TRAJECTORY_DETAIL_WINDOW_BYTES),
                                None,
                            )
                        })
                        .await;

                    let _ = waku.update(cx, |waku, cx| {
                        let mut sessions = waku.trajectory_sessions.borrow_mut();
                        if let Some(state) = sessions.get_mut(&session_id) {
                            match result {
                                Ok(wakuwaku_protocol::TrajectoryResponse::Detail {
                                    content,
                                    ..
                                }) => {
                                    state.detail_cache.fulfill(token, content);
                                    cx.notify();
                                }
                                _ => {
                                    state.detail_cache.abandon(token);
                                }
                            }
                        }
                    });
                })
                .detach();

                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("trajectory.loading"))
                    .into_any_element()
            }
            Query::Pending => {
                drop(trajectory_sessions);
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("trajectory.loading"))
                    .into_any_element()
            }
        };

        div()
            .flex_1()
            .flex()
            .flex_col()
            .min_h_0()
            .min_w_0()
            .bg(theme.surface)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key.as_str() == "escape" {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.close_inspector();
                        cx.notify();
                    }
                }
            }))
            .child(header)
            .child(
                div()
                    .id("trajectory-inspector-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .min_h_0()
                    .child(content_element),
            )
    }

    fn render_inspector_content(
        &mut self,
        session_id: Uuid,
        record: &TrajectoryRowSummary,
        section: TrajectoryDetailSection,
        content: &TrajectoryDetailContent,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut root = div().p(px(14.0)).flex().flex_col().gap(px(12.0));

        // Top summary metadata bar
        let meta_bar = div()
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .p(px(10.0))
            .rounded(px(6.0))
            .bg(theme.raised)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .child(format!("Kind: {}", kind_display_name(record.kind))),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .child(format!("Status: {}", status_display_name(record.status))),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary)
                    .child(format!(
                        "Duration: {}",
                        format_exact_duration(record.duration_ms)
                    )),
            );
        root = root.child(meta_bar);

        // Render main content: JSON tree vs Markdown / text
        if let Some(ref json_val) = content.json {
            let tree_element = self.render_json_tree(session_id, json_val, &theme, cx);
            root = root.child(tree_element);
        } else if let Some(text) = &content.text {
            let markdown_element = self.render_detail_markdown(text, &theme, cx);
            root = root.child(markdown_element);
        } else {
            root = root.child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.text_tertiary)
                    .child("No content"),
            );
        }

        // Chunk pagination ("Load more")
        if content.byte_length < content.total_bytes {
            let record_id = record.record_id;
            let loaded = content.byte_length;
            let total = content.total_bytes;
            let next_offset = content.offset + content.byte_length;

            root = root.child(
                div()
                    .mt(px(8.0))
                    .p(px(10.0))
                    .rounded(px(6.0))
                    .bg(theme.raised)
                    .border_1()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.text_secondary)
                            .child(tr!(
                                "trajectory.bytes_loaded",
                                loaded = loaded,
                                total = total
                            )),
                    )
                    .child(
                        div()
                            .id("inspector-load-more-btn")
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(4.0))
                            .bg(theme.accent)
                            .text_color(theme.text)
                            .text_size(px(12.0))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.9))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                let mut sessions = this.trajectory_sessions.borrow_mut();
                                if let Some(state) = sessions.get_mut(&session_id) {
                                    state
                                        .detail_cursors
                                        .insert((record_id, section), next_offset as u64);
                                    cx.notify();
                                }
                            }))
                            .child(tr!("trajectory.load_more")),
                    ),
            );
        }

        root.into_any_element()
    }

    fn render_detail_markdown(
        &mut self,
        text: &str,
        theme: &Theme,
        _cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = MarkdownPalette::from_theme(theme);
        let mut view = MarkdownView::new();
        view.set_text(text, false);
        let ctx = MarkdownCtx::new(
            "trajectory-detail-markdown",
            &palette,
            MarkdownMetrics::COMPACT,
            self.transcript_selection.clone(),
        );

        div()
            .text_size(px(13.0))
            .line_height(px(20.0))
            .text_color(theme.text)
            .children(crate::md::render::markdown(&view, &ctx))
            .into_any_element()
    }

    fn render_json_tree(
        &mut self,
        session_id: Uuid,
        value: &Value,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut sessions = self.trajectory_sessions.borrow_mut();
        let Some(state) = sessions.get_mut(&session_id) else {
            return div().into_any_element();
        };

        let nodes = flatten_json_tree(value, &state.json_tree_state.expanded_paths);
        state.json_tree_state.flattened_nodes = nodes.clone();

        let mut list = div()
            .id("trajectory-json-tree")
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(8.0))
            .rounded(px(6.0))
            .bg(theme.raised)
            .border_1()
            .border_color(theme.border)
            .font_family(crate::md::render::MONO_FAMILY)
            .text_size(px(12.0));

        let selected_idx = state.json_tree_state.selected_index;

        for (idx, node) in nodes.iter().enumerate() {
            let is_selected = idx == selected_idx;
            let path_clone = node.path.clone();
            let is_expandable = node.expandable;
            let is_expanded = node.expanded;

            let row = div()
                .id(SharedString::from(format!("json-node-{}", node.id)))
                .flex()
                .items_center()
                .gap(px(4.0))
                .py(px(2.0))
                .px(px(4.0))
                .pl(px(4.0 + (node.depth as f32 * 14.0)))
                .rounded(px(3.0))
                .bg(if is_selected {
                    theme.accent.opacity(0.12)
                } else {
                    gpui::hsla(0.0, 0.0, 0.0, 0.0)
                })
                .hover(|s| s.bg(theme.accent.opacity(0.06)))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let mut sessions = this.trajectory_sessions.borrow_mut();
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.json_tree_state.selected_index = idx;
                        if is_expandable {
                            if is_expanded {
                                state.json_tree_state.expanded_paths.remove(&path_clone);
                            } else {
                                state
                                    .json_tree_state
                                    .expanded_paths
                                    .insert(path_clone.clone());
                            }
                        }
                        cx.notify();
                    }
                }))
                .when(is_expandable, |element| {
                    element.child(
                        div()
                            .w(px(12.0))
                            .text_color(theme.text_tertiary)
                            .child(if is_expanded { "▼" } else { "▶" }),
                    )
                })
                .when(!is_expandable, |element| element.child(div().w(px(12.0))))
                .when_some(node.key.as_ref(), |element, key| {
                    element.child(div().text_color(theme.accent).child(format!("{key}: ")))
                })
                .child(
                    div()
                        .text_color(match node.value_type {
                            JsonValueType::Object | JsonValueType::Array => theme.text_secondary,
                            JsonValueType::String => theme.text,
                            JsonValueType::Number => theme.accent,
                            JsonValueType::Boolean => theme.accent,
                            JsonValueType::Null => theme.text_tertiary,
                        })
                        .child(node.value_preview.clone()),
                );

            list = list.child(row);
        }

        // Keyboard handler for JSON tree
        let nodes_len = nodes.len();
        list = list.on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
            let mut sessions = this.trajectory_sessions.borrow_mut();
            let Some(state) = sessions.get_mut(&session_id) else {
                return;
            };

            match event.keystroke.key.as_str() {
                "up" => {
                    if state.json_tree_state.selected_index > 0 {
                        state.json_tree_state.selected_index -= 1;
                        cx.notify();
                    }
                }
                "down" => {
                    if state.json_tree_state.selected_index + 1 < nodes_len {
                        state.json_tree_state.selected_index += 1;
                        cx.notify();
                    }
                }
                "left" => {
                    let idx = state.json_tree_state.selected_index;
                    if let Some(node) = state.json_tree_state.flattened_nodes.get(idx) {
                        if node.expandable && node.expanded {
                            state.json_tree_state.expanded_paths.remove(&node.path);
                            cx.notify();
                        }
                    }
                }
                "right" => {
                    let idx = state.json_tree_state.selected_index;
                    if let Some(node) = state.json_tree_state.flattened_nodes.get(idx) {
                        if node.expandable && !node.expanded {
                            state
                                .json_tree_state
                                .expanded_paths
                                .insert(node.path.clone());
                            cx.notify();
                        }
                    }
                }
                "enter" | "space" => {
                    let idx = state.json_tree_state.selected_index;
                    if let Some(node) = state.json_tree_state.flattened_nodes.get(idx) {
                        if node.expandable {
                            if node.expanded {
                                state.json_tree_state.expanded_paths.remove(&node.path);
                            } else {
                                state
                                    .json_tree_state
                                    .expanded_paths
                                    .insert(node.path.clone());
                            }
                            cx.notify();
                        }
                    }
                }
                _ => {}
            }
        }));

        list.into_any_element()
    }
}

fn localized_section_name(section: TrajectoryDetailSection) -> String {
    match section {
        TrajectoryDetailSection::Summary => tr!("trajectory.tab_summary"),
        TrajectoryDetailSection::Preview => tr!("trajectory.tab_preview"),
        TrajectoryDetailSection::Raw => tr!("trajectory.tab_raw"),
        TrajectoryDetailSection::Source => tr!("trajectory.tab_source"),
        TrajectoryDetailSection::SystemPrompt => tr!("trajectory.tab_system_prompt"),
        TrajectoryDetailSection::Tools => tr!("trajectory.tab_tools"),
        TrajectoryDetailSection::Diff => tr!("trajectory.tab_diff"),
        TrajectoryDetailSection::Options => tr!("trajectory.tab_options"),
        TrajectoryDetailSection::Usage => tr!("trajectory.tab_usage"),
        TrajectoryDetailSection::Timing => tr!("trajectory.tab_timing"),
        TrajectoryDetailSection::Payload => tr!("trajectory.tab_payload"),
        TrajectoryDetailSection::Result => tr!("trajectory.tab_result"),
        TrajectoryDetailSection::Schema => tr!("trajectory.tab_schema"),
    }
}
