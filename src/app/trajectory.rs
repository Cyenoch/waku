use std::collections::{HashMap, HashSet};

use crate::ui::scrollbar::ScrollbarState;
use gpui::ListState;

use serde_json::Value;
use uuid::Uuid;
use wakuwaku_protocol::{
    TrajectoryAvailability, TrajectoryCursor, TrajectoryDetailContent, TrajectoryDetailSection,
    TrajectoryKind, TrajectoryLane, TrajectoryLiveUpdate, TrajectoryRowSummary, TrajectoryStatus,
};

use crate::query::QueryCache;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrajectoryLoadingStatus {
    #[default]
    Initial,
    Loading,
    Ready,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrajectoryLedgerRowKind {
    System {
        record: TrajectoryRowSummary,
    },
    TurnDivider {
        turn_count: u32,
        collapsed: bool,
        record_count: usize,
        total_duration_ms: Option<i64>,
    },
    StepRequest {
        record: TrajectoryRowSummary,
        children_count: usize,
    },
    Assistant {
        record: TrajectoryRowSummary,
    },
    Tool {
        record: TrajectoryRowSummary,
    },
    Context {
        record: TrajectoryRowSummary,
    },
    ToolOnlyPlaceholder {
        parent_record_id: Option<Uuid>,
        step: u32,
        turn_count: u32,
        duration_ms: Option<i64>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrajectoryLedgerRow {
    pub key: String,
    pub record_id: Option<Uuid>,
    pub kind: TrajectoryLedgerRowKind,
    pub depth: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TimelineSpan {
    pub record_id: Uuid,
    pub lane: TrajectoryLane,
    pub kind: TrajectoryKind,
    pub status: TrajectoryStatus,
    pub title: String,
    pub start_pct: f32,
    pub width_pct: f32,
    pub has_timing: bool,
    pub duration_text: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineLayout {
    pub input_spans: Vec<TimelineSpan>,
    pub model_spans: Vec<TimelineSpan>,
    pub tools_spans: Vec<TimelineSpan>,
    pub min_time_ms: i64,
    pub max_time_ms: i64,
    pub total_duration_ms: i64,
    pub has_any_timing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum JsonValueType {
    Object,
    Array,
    String,
    Number,
    Boolean,
    Null,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonTreeNode {
    pub id: String,
    pub path: String,
    pub key: Option<String>,
    pub value_preview: String,
    pub value_type: JsonValueType,
    pub depth: usize,
    pub expandable: bool,
    pub expanded: bool,
    pub raw_value: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct JsonTreeState {
    pub expanded_paths: HashSet<String>,
    pub selected_index: usize,
    pub flattened_nodes: Vec<JsonTreeNode>,
}

#[allow(dead_code)]
pub struct TrajectorySessionState {
    pub session_id: Uuid,
    pub generation: u64,
    pub revision: u64,
    pub availability: TrajectoryAvailability,
    pub records: Vec<TrajectoryRowSummary>,
    pub older_cursor: Option<TrajectoryCursor>,
    pub newer_cursor: Option<TrajectoryCursor>,
    pub has_older: bool,
    pub has_newer: bool,
    pub loading_status: TrajectoryLoadingStatus,
    pub loading_older: bool,
    pub error: Option<String>,

    // UI state
    pub search_query: String,
    pub search_tokens: Vec<String>,
    pub prebuilt_search_index: HashMap<Uuid, String>,
    pub duration_projection: bool,
    pub folded_turns: HashSet<u32>,
    pub show_tool_calls: bool,
    pub selected_record_id: Option<Uuid>,
    pub selected_row_index: Option<usize>,
    pub inspector_open: bool,
    pub inspector_width: f32,
    pub selected_section: TrajectoryDetailSection,
    pub timeline_layout: TimelineLayout,
    pub ledger_rows: Vec<TrajectoryLedgerRow>,
    pub list_state: ListState,
    pub scrollbar_state: std::rc::Rc<ScrollbarState>,
    pub detail_cache: QueryCache<(Uuid, TrajectoryDetailSection, u64), TrajectoryDetailContent>,
    pub detail_cursors: HashMap<(Uuid, TrajectoryDetailSection), u64>,
    pub json_tree_state: JsonTreeState,
}

impl TrajectorySessionState {
    pub fn new(session_id: Uuid) -> Self {
        let list_state = ListState::new(0, gpui::ListAlignment::Bottom, gpui::px(100.0));
        let mut state = Self {
            session_id,
            generation: 0,
            revision: 0,
            availability: TrajectoryAvailability::Exact,
            records: Vec::new(),
            older_cursor: None,
            newer_cursor: None,
            has_older: false,
            has_newer: false,
            loading_status: TrajectoryLoadingStatus::Initial,
            loading_older: false,
            error: None,

            search_query: String::new(),
            search_tokens: Vec::new(),
            prebuilt_search_index: HashMap::new(),
            duration_projection: true,
            folded_turns: HashSet::new(),
            show_tool_calls: true,
            selected_record_id: None,
            selected_row_index: None,
            inspector_open: false,
            inspector_width: 380.0,
            selected_section: TrajectoryDetailSection::Summary,
            timeline_layout: TimelineLayout::default(),
            ledger_rows: Vec::new(),
            list_state,
            scrollbar_state: ScrollbarState::new(),
            detail_cache: QueryCache::new(64),
            detail_cursors: HashMap::new(),
            json_tree_state: JsonTreeState::default(),
        };
        state.rebuild_all();
        state
    }

    pub fn reset_generation(&mut self, generation: u64, revision: u64) {
        self.generation = generation;
        self.revision = revision;
        self.records.clear();
        self.older_cursor = None;
        self.newer_cursor = None;
        self.has_older = false;
        self.has_newer = false;
        self.prebuilt_search_index.clear();
        self.detail_cache.clear();
        self.detail_cursors.clear();
        self.rebuild_all();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_page_response(
        &mut self,
        availability: TrajectoryAvailability,
        generation: u64,
        revision: u64,
        rows: Vec<TrajectoryRowSummary>,
        older: Option<TrajectoryCursor>,
        newer: Option<TrajectoryCursor>,
        has_older: bool,
        has_newer: bool,
    ) {
        // Stale token rejection
        if generation < self.generation {
            return;
        }
        if generation > self.generation {
            self.generation = generation;
            self.records.clear();
        }
        self.availability = availability;
        self.revision = self.revision.max(revision);
        self.records = rows;
        self.older_cursor = older;
        self.newer_cursor = newer;
        self.has_older = has_older;
        self.has_newer = has_newer;
        self.loading_status = TrajectoryLoadingStatus::Ready;
        self.error = None;
        self.rebuild_all();
    }

    pub fn prepend_older_page(
        &mut self,
        older_rows: Vec<TrajectoryRowSummary>,
        older_cursor: Option<TrajectoryCursor>,
        has_older: bool,
    ) -> Option<Uuid> {
        let anchor_id = self.selected_record_id.or_else(|| {
            self.selected_row_index
                .and_then(|idx| self.ledger_rows.get(idx))
                .and_then(|row| row.record_id)
        });

        let mut existing_ids: HashSet<Uuid> = self.records.iter().map(|r| r.record_id).collect();
        let mut prepended = Vec::new();
        for row in older_rows {
            if existing_ids.insert(row.record_id) {
                prepended.push(row);
            }
        }
        prepended.extend(std::mem::take(&mut self.records));
        prepended.sort_by_key(|r| (r.sequence, r.step));
        self.records = prepended;
        self.older_cursor = older_cursor;
        self.has_older = has_older;
        self.loading_older = false;
        self.rebuild_all();
        anchor_id
    }

    pub fn apply_live_update(&mut self, update: TrajectoryLiveUpdate) {
        match update {
            TrajectoryLiveUpdate::Upsert {
                generation,
                revision,
                row,
            } => {
                if generation < self.generation {
                    return;
                }
                if generation > self.generation {
                    self.reset_generation(generation, revision);
                }
                self.revision = self.revision.max(revision);
                let row = *row;
                let record_id = row.record_id;
                if let Some(pos) = self.records.iter().position(|r| r.record_id == record_id) {
                    self.records[pos] = row;
                } else {
                    self.records.push(row);
                    self.records.sort_by_key(|r| (r.sequence, r.step));
                }
                self.rebuild_all();
            }
            TrajectoryLiveUpdate::Remove {
                generation,
                revision,
                record_id,
            } => {
                if generation < self.generation {
                    return;
                }
                self.revision = self.revision.max(revision);
                self.records.retain(|r| r.record_id != record_id);
                self.rebuild_all();
            }
            TrajectoryLiveUpdate::Reset {
                generation,
                revision,
            } => {
                self.reset_generation(generation, revision);
            }
        }
    }

    pub fn set_search_query(&mut self, query: String) {
        self.search_query = query;
        self.search_tokens = self
            .search_query
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect();
        self.rebuild_ledger_rows();
    }

    pub fn toggle_duration_projection(&mut self) {
        self.duration_projection = !self.duration_projection;
    }

    pub fn toggle_tool_calls(&mut self) {
        self.show_tool_calls = !self.show_tool_calls;
        self.rebuild_ledger_rows();
    }

    pub fn fold_all_turns(&mut self) {
        let all_turns: HashSet<u32> = self.records.iter().map(|r| r.turn_count).collect();
        self.folded_turns = all_turns;
        self.rebuild_ledger_rows();
    }

    pub fn unfold_all_turns(&mut self) {
        self.folded_turns.clear();
        self.rebuild_ledger_rows();
    }

    pub fn toggle_turn_fold(&mut self, turn_count: u32) {
        if !self.folded_turns.remove(&turn_count) {
            self.folded_turns.insert(turn_count);
        }
        self.rebuild_ledger_rows();
    }

    pub fn select_record(&mut self, record_id: Uuid) {
        self.selected_record_id = Some(record_id);
        self.selected_row_index = self
            .ledger_rows
            .iter()
            .position(|r| r.record_id == Some(record_id));
        if let Some(idx) = self.selected_row_index {
            self.list_state.scroll_to_reveal_item(idx);
        }
        self.inspector_open = true;
        self.selected_section = self
            .default_section_for_record(record_id)
            .unwrap_or(TrajectoryDetailSection::Summary);
    }

    pub fn select_row_at_index(&mut self, index: usize) {
        if index < self.ledger_rows.len() {
            self.selected_row_index = Some(index);
            self.list_state.scroll_to_reveal_item(index);
            if let Some(record_id) = self.ledger_rows[index].record_id {
                self.selected_record_id = Some(record_id);
                self.selected_section = self
                    .default_section_for_record(record_id)
                    .unwrap_or(TrajectoryDetailSection::Summary);
            }
        }
    }

    pub fn close_inspector(&mut self) {
        self.inspector_open = false;
        if let Some(idx) = self.selected_row_index {
            self.list_state.scroll_to_reveal_item(idx);
        }
    }

    pub fn default_section_for_record(&self, record_id: Uuid) -> Option<TrajectoryDetailSection> {
        let record = self.records.iter().find(|r| r.record_id == record_id)?;
        Some(default_section_for_kind(record.kind))
    }

    pub fn allowed_sections_for_record(&self, record_id: Uuid) -> Vec<TrajectoryDetailSection> {
        let Some(record) = self.records.iter().find(|r| r.record_id == record_id) else {
            return vec![TrajectoryDetailSection::Summary];
        };
        allowed_sections_for_kind(record.kind)
    }

    pub fn rebuild_all(&mut self) {
        self.rebuild_search_index();
        self.rebuild_timeline_layout();
        self.rebuild_ledger_rows();
    }

    pub fn rebuild_search_index(&mut self) {
        self.prebuilt_search_index.clear();
        for record in &self.records {
            let mut search_blob = String::with_capacity(
                record.title.len() + record.preview.len() + record.search_text.len() + 64,
            );
            search_blob.push_str(&record.title);
            search_blob.push(' ');
            search_blob.push_str(&record.preview);
            search_blob.push(' ');
            search_blob.push_str(&record.search_text);
            search_blob.push(' ');
            search_blob.push_str(kind_display_name(record.kind));
            search_blob.push(' ');
            search_blob.push_str(status_display_name(record.status));
            search_blob.push(' ');
            search_blob.push_str(lane_display_name(record.lane));
            self.prebuilt_search_index
                .insert(record.record_id, search_blob.to_lowercase());
        }
    }

    pub fn rebuild_timeline_layout(&mut self) {
        self.timeline_layout = compute_timeline_layout(&self.records);
    }

    pub fn rebuild_ledger_rows(&mut self) {
        self.ledger_rows = build_ledger_rows(
            &self.records,
            &self.folded_turns,
            self.show_tool_calls,
            &self.search_tokens,
            &self.prebuilt_search_index,
        );
        let count = self.ledger_rows.len();
        self.list_state.reset(count);

        if let Some(record_id) = self.selected_record_id {
            self.selected_row_index = self
                .ledger_rows
                .iter()
                .position(|r| r.record_id == Some(record_id));
        } else if let Some(idx) = self.selected_row_index
            && idx >= count
        {
            self.selected_row_index = count.checked_sub(1);
        }
    }
}

pub fn default_section_for_kind(kind: TrajectoryKind) -> TrajectoryDetailSection {
    match kind {
        TrajectoryKind::System => TrajectoryDetailSection::Summary,
        TrajectoryKind::User => TrajectoryDetailSection::Preview,
        TrajectoryKind::Context => TrajectoryDetailSection::Summary,
        TrajectoryKind::Request => TrajectoryDetailSection::Summary,
        TrajectoryKind::Assistant => TrajectoryDetailSection::Summary,
        TrajectoryKind::Tool => TrajectoryDetailSection::Summary,
    }
}

pub fn allowed_sections_for_kind(kind: TrajectoryKind) -> Vec<TrajectoryDetailSection> {
    match kind {
        TrajectoryKind::System => vec![
            TrajectoryDetailSection::Summary,
            TrajectoryDetailSection::SystemPrompt,
            TrajectoryDetailSection::Source,
            TrajectoryDetailSection::Raw,
        ],
        TrajectoryKind::User => vec![
            TrajectoryDetailSection::Summary,
            TrajectoryDetailSection::Preview,
            TrajectoryDetailSection::Source,
            TrajectoryDetailSection::Raw,
        ],
        TrajectoryKind::Context => vec![
            TrajectoryDetailSection::Summary,
            TrajectoryDetailSection::Preview,
            TrajectoryDetailSection::Diff,
            TrajectoryDetailSection::Source,
            TrajectoryDetailSection::Raw,
        ],
        TrajectoryKind::Request => vec![
            TrajectoryDetailSection::Summary,
            TrajectoryDetailSection::SystemPrompt,
            TrajectoryDetailSection::Tools,
            TrajectoryDetailSection::Options,
            TrajectoryDetailSection::Usage,
            TrajectoryDetailSection::Timing,
            TrajectoryDetailSection::Source,
            TrajectoryDetailSection::Raw,
        ],
        TrajectoryKind::Assistant => vec![
            TrajectoryDetailSection::Summary,
            TrajectoryDetailSection::Preview,
            TrajectoryDetailSection::Usage,
            TrajectoryDetailSection::Timing,
            TrajectoryDetailSection::Source,
            TrajectoryDetailSection::Raw,
        ],
        TrajectoryKind::Tool => vec![
            TrajectoryDetailSection::Summary,
            TrajectoryDetailSection::Payload,
            TrajectoryDetailSection::Result,
            TrajectoryDetailSection::Diff,
            TrajectoryDetailSection::Schema,
            TrajectoryDetailSection::Timing,
            TrajectoryDetailSection::Source,
            TrajectoryDetailSection::Raw,
        ],
    }
}

pub fn kind_display_name(kind: TrajectoryKind) -> &'static str {
    match kind {
        TrajectoryKind::System => "system",
        TrajectoryKind::User => "user",
        TrajectoryKind::Context => "context",
        TrajectoryKind::Request => "request",
        TrajectoryKind::Assistant => "assistant",
        TrajectoryKind::Tool => "tool",
    }
}

pub fn status_display_name(status: TrajectoryStatus) -> &'static str {
    match status {
        TrajectoryStatus::Pending => "pending",
        TrajectoryStatus::Running => "running",
        TrajectoryStatus::Completed => "completed",
        TrajectoryStatus::Failed => "failed",
        TrajectoryStatus::Cancelled => "cancelled",
        TrajectoryStatus::Unavailable => "unavailable",
    }
}

pub fn lane_display_name(lane: TrajectoryLane) -> &'static str {
    match lane {
        TrajectoryLane::Input => "input",
        TrajectoryLane::Model => "model",
        TrajectoryLane::Tools => "tools",
    }
}

pub fn format_exact_duration(duration_ms: Option<i64>) -> String {
    let Some(ms) = duration_ms else {
        return tr!("trajectory.no_timing_data");
    };
    if ms < 0 {
        return tr!("trajectory.no_timing_data");
    }
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m {secs}s")
    } else {
        let hrs = ms / 3_600_000;
        let mins = (ms % 3_600_000) / 60_000;
        let secs = (ms % 60_000) / 1000;
        if secs > 0 {
            format!("{hrs}h {mins}m {secs}s")
        } else {
            format!("{hrs}h {mins}m")
        }
    }
}

pub fn matches_and_search(
    record_id: Uuid,
    tokens: &[String],
    search_index: &HashMap<Uuid, String>,
) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let Some(indexed) = search_index.get(&record_id) else {
        return false;
    };
    tokens.iter().all(|token| indexed.contains(token))
}

pub fn compute_timeline_layout(records: &[TrajectoryRowSummary]) -> TimelineLayout {
    let mut min_time_ms = i64::MAX;
    let mut max_time_ms = i64::MIN;
    let mut has_any_timing = false;

    // First pass: find global bounds
    for r in records {
        if let Some(start) = r.started_at_ms {
            has_any_timing = true;
            min_time_ms = min_time_ms.min(start);
            let end = r
                .completed_at_ms
                .or_else(|| r.duration_ms.map(|d| start.saturating_add(d)))
                .unwrap_or(start);
            max_time_ms = max_time_ms.max(end);
        } else if let Some(comp) = r.completed_at_ms {
            has_any_timing = true;
            min_time_ms = min_time_ms.min(comp);
            max_time_ms = max_time_ms.max(comp);
        }
    }

    if !has_any_timing || min_time_ms > max_time_ms {
        let default_span = |r: &TrajectoryRowSummary, idx: usize, total: usize| TimelineSpan {
            record_id: r.record_id,
            lane: r.lane,
            kind: r.kind,
            status: r.status,
            title: r.title.clone(),
            start_pct: if total > 1 {
                (idx as f32) / (total as f32)
            } else {
                0.0
            },
            width_pct: if total > 0 {
                (1.0 / (total as f32)).max(0.04)
            } else {
                1.0
            },
            has_timing: false,
            duration_text: format_exact_duration(r.duration_ms),
        };

        let mut input_spans = Vec::new();
        let mut model_spans = Vec::new();
        let mut tools_spans = Vec::new();
        let total = records.len();
        for (idx, r) in records.iter().enumerate() {
            let span = default_span(r, idx, total);
            match r.lane {
                TrajectoryLane::Input => input_spans.push(span),
                TrajectoryLane::Model => model_spans.push(span),
                TrajectoryLane::Tools => tools_spans.push(span),
            }
        }

        return TimelineLayout {
            input_spans,
            model_spans,
            tools_spans,
            min_time_ms: 0,
            max_time_ms: 0,
            total_duration_ms: 0,
            has_any_timing: false,
        };
    }

    let total_duration_ms = (max_time_ms - min_time_ms).max(1);

    // Build map from request_id -> (start_ms, end_ms) for Assistant reusing parent Request timing
    let mut request_timings: HashMap<Uuid, (i64, i64)> = HashMap::new();
    for r in records {
        if r.kind == TrajectoryKind::Request
            && let Some(start) = r.started_at_ms
        {
            let end = r
                .completed_at_ms
                .or_else(|| r.duration_ms.map(|d| start.saturating_add(d)))
                .unwrap_or(start);
            request_timings.insert(r.record_id, (start, end));
        }
    }

    let mut input_spans = Vec::new();
    let mut model_spans = Vec::new();
    let mut tools_spans = Vec::new();

    for r in records {
        let (start, end, has_timing) = if let Some(start) = r.started_at_ms {
            let end = r
                .completed_at_ms
                .or_else(|| r.duration_ms.map(|d| start.saturating_add(d)))
                .unwrap_or(start);
            (start, end, true)
        } else if r.kind == TrajectoryKind::Assistant
            && let Some(parent_id) = r.parent_record_id.or(r.request_id)
            && let Some(&(req_start, req_end)) = request_timings.get(&parent_id)
        {
            (req_start, req_end, true)
        } else if let Some(comp) = r.completed_at_ms {
            (comp, comp, true)
        } else {
            (min_time_ms, min_time_ms, false)
        };

        let start_pct = ((start - min_time_ms) as f32 / total_duration_ms as f32).clamp(0.0, 1.0);
        let duration_span = (end - start).max(0);
        let raw_width_pct = (duration_span as f32 / total_duration_ms as f32).clamp(0.0, 1.0);
        let width_pct = raw_width_pct.max(0.02).min(1.0 - start_pct + 0.02);

        let span = TimelineSpan {
            record_id: r.record_id,
            lane: r.lane,
            kind: r.kind,
            status: r.status,
            title: r.title.clone(),
            start_pct,
            width_pct,
            has_timing,
            duration_text: format_exact_duration(r.duration_ms),
        };

        match r.lane {
            TrajectoryLane::Input => input_spans.push(span),
            TrajectoryLane::Model => model_spans.push(span),
            TrajectoryLane::Tools => tools_spans.push(span),
        }
    }

    TimelineLayout {
        input_spans,
        model_spans,
        tools_spans,
        min_time_ms,
        max_time_ms,
        total_duration_ms,
        has_any_timing: true,
    }
}

pub fn build_ledger_rows(
    records: &[TrajectoryRowSummary],
    folded_turns: &HashSet<u32>,
    show_tool_calls: bool,
    search_tokens: &[String],
    search_index: &HashMap<Uuid, String>,
) -> Vec<TrajectoryLedgerRow> {
    let mut rows = Vec::new();
    if records.is_empty() {
        return rows;
    }

    // Group records by turn
    let mut turns_map: HashMap<u32, Vec<&TrajectoryRowSummary>> = HashMap::new();
    let mut turns_order: Vec<u32> = Vec::new();

    for record in records {
        if !turns_map.contains_key(&record.turn_count) {
            turns_order.push(record.turn_count);
        }
        turns_map.entry(record.turn_count).or_default().push(record);
    }

    for &turn in &turns_order {
        let turn_records = turns_map.get(&turn).unwrap();
        let is_folded = folded_turns.contains(&turn);

        // Turn summary stats
        let record_count = turn_records.len();
        let total_duration_ms = turn_records
            .iter()
            .filter_map(|r| r.duration_ms)
            .reduce(|a, b| a.saturating_add(b));

        // Emit Turn Divider
        rows.push(TrajectoryLedgerRow {
            key: format!("turn-divider-{turn}"),
            record_id: None,
            kind: TrajectoryLedgerRowKind::TurnDivider {
                turn_count: turn,
                collapsed: is_folded,
                record_count,
                total_duration_ms,
            },
            depth: 0,
        });

        if is_folded {
            continue;
        }

        // Process records in this turn
        let mut i = 0;
        while i < turn_records.len() {
            let record = turn_records[i];
            let matches = matches_and_search(record.record_id, search_tokens, search_index);

            match record.kind {
                TrajectoryKind::System => {
                    if matches {
                        rows.push(TrajectoryLedgerRow {
                            key: format!("system-{}", record.record_id),
                            record_id: Some(record.record_id),
                            kind: TrajectoryLedgerRowKind::System {
                                record: (*record).clone(),
                            },
                            depth: 1,
                        });
                    }
                    i += 1;
                }
                TrajectoryKind::Context => {
                    if matches {
                        rows.push(TrajectoryLedgerRow {
                            key: format!("context-{}", record.record_id),
                            record_id: Some(record.record_id),
                            kind: TrajectoryLedgerRowKind::Context {
                                record: (*record).clone(),
                            },
                            depth: 1,
                        });
                    }
                    i += 1;
                }
                TrajectoryKind::User => {
                    if matches {
                        rows.push(TrajectoryLedgerRow {
                            key: format!("user-{}", record.record_id),
                            record_id: Some(record.record_id),
                            kind: TrajectoryLedgerRowKind::Context {
                                record: (*record).clone(),
                            },
                            depth: 1,
                        });
                    }
                    i += 1;
                }
                TrajectoryKind::Request => {
                    // Collect children (Assistant or Tool) associated with this request
                    let req_id = record.record_id;
                    let mut children = Vec::new();
                    let mut j = i + 1;
                    while j < turn_records.len() {
                        let next = turn_records[j];
                        if next.kind == TrajectoryKind::Request
                            || next.kind == TrajectoryKind::System
                        {
                            break;
                        }
                        if next.parent_record_id == Some(req_id) || next.request_id == Some(req_id)
                        {
                            children.push(next);
                            j += 1;
                        } else {
                            break;
                        }
                    }

                    let children_count = children.len();
                    if matches {
                        rows.push(TrajectoryLedgerRow {
                            key: format!("request-{}", record.record_id),
                            record_id: Some(record.record_id),
                            kind: TrajectoryLedgerRowKind::StepRequest {
                                record: (*record).clone(),
                                children_count,
                            },
                            depth: 1,
                        });
                    }

                    // Check if there are tool children but NO assistant commentary
                    let has_assistant =
                        children.iter().any(|c| c.kind == TrajectoryKind::Assistant);
                    let has_tools = children.iter().any(|c| c.kind == TrajectoryKind::Tool);

                    if has_tools && !has_assistant && show_tool_calls {
                        let matches_placeholder = search_tokens.is_empty();
                        if matches_placeholder {
                            rows.push(TrajectoryLedgerRow {
                                key: format!("tool-only-placeholder-{}", record.record_id),
                                record_id: Some(record.record_id),
                                kind: TrajectoryLedgerRowKind::ToolOnlyPlaceholder {
                                    parent_record_id: Some(record.record_id),
                                    step: record.step,
                                    turn_count: record.turn_count,
                                    duration_ms: record.duration_ms,
                                },
                                depth: 2,
                            });
                        }
                    }

                    for child in children {
                        let child_matches =
                            matches_and_search(child.record_id, search_tokens, search_index);
                        if child.kind == TrajectoryKind::Assistant {
                            if child_matches {
                                rows.push(TrajectoryLedgerRow {
                                    key: format!("assistant-{}", child.record_id),
                                    record_id: Some(child.record_id),
                                    kind: TrajectoryLedgerRowKind::Assistant {
                                        record: (*child).clone(),
                                    },
                                    depth: 2,
                                });
                            }
                        } else if child.kind == TrajectoryKind::Tool
                            && show_tool_calls
                            && child_matches
                        {
                            rows.push(TrajectoryLedgerRow {
                                key: format!("tool-{}", child.record_id),
                                record_id: Some(child.record_id),
                                kind: TrajectoryLedgerRowKind::Tool {
                                    record: (*child).clone(),
                                },
                                depth: 2,
                            });
                        }
                    }

                    i = j;
                }
                TrajectoryKind::Assistant => {
                    if matches {
                        rows.push(TrajectoryLedgerRow {
                            key: format!("assistant-{}", record.record_id),
                            record_id: Some(record.record_id),
                            kind: TrajectoryLedgerRowKind::Assistant {
                                record: (*record).clone(),
                            },
                            depth: 1,
                        });
                    }
                    i += 1;
                }
                TrajectoryKind::Tool => {
                    if show_tool_calls && matches {
                        rows.push(TrajectoryLedgerRow {
                            key: format!("tool-{}", record.record_id),
                            record_id: Some(record.record_id),
                            kind: TrajectoryLedgerRowKind::Tool {
                                record: (*record).clone(),
                            },
                            depth: 1,
                        });
                    }
                    i += 1;
                }
            }
        }
    }

    rows
}

pub fn flatten_json_tree(value: &Value, expanded_paths: &HashSet<String>) -> Vec<JsonTreeNode> {
    let mut nodes = Vec::new();
    flatten_json_value(value, "$", None, 0, expanded_paths, &mut nodes);
    nodes
}

fn flatten_json_value(
    value: &Value,
    path: &str,
    key: Option<&str>,
    depth: usize,
    expanded_paths: &HashSet<String>,
    out: &mut Vec<JsonTreeNode>,
) {
    match value {
        Value::Object(map) => {
            let expanded = expanded_paths.contains(path);
            let preview = format!("Object ({} entries)", map.len());
            out.push(JsonTreeNode {
                id: path.to_string(),
                path: path.to_string(),
                key: key.map(String::from),
                value_preview: preview,
                value_type: JsonValueType::Object,
                depth,
                expandable: !map.is_empty(),
                expanded,
                raw_value: Some(value.clone()),
            });
            if expanded {
                for (k, v) in map {
                    let child_path = format!("{path}.{k}");
                    flatten_json_value(v, &child_path, Some(k), depth + 1, expanded_paths, out);
                }
            }
        }
        Value::Array(items) => {
            let expanded = expanded_paths.contains(path);
            let preview = format!("Array ({} items)", items.len());
            out.push(JsonTreeNode {
                id: path.to_string(),
                path: path.to_string(),
                key: key.map(String::from),
                value_preview: preview,
                value_type: JsonValueType::Array,
                depth,
                expandable: !items.is_empty(),
                expanded,
                raw_value: Some(value.clone()),
            });
            if expanded {
                for (idx, item) in items.iter().enumerate() {
                    let child_path = format!("{path}[{idx}]");
                    let k_label = format!("[{idx}]");
                    flatten_json_value(
                        item,
                        &child_path,
                        Some(&k_label),
                        depth + 1,
                        expanded_paths,
                        out,
                    );
                }
            }
        }
        Value::String(s) => {
            out.push(JsonTreeNode {
                id: path.to_string(),
                path: path.to_string(),
                key: key.map(String::from),
                value_preview: format!("\"{s}\""),
                value_type: JsonValueType::String,
                depth,
                expandable: false,
                expanded: false,
                raw_value: Some(value.clone()),
            });
        }
        Value::Number(n) => {
            out.push(JsonTreeNode {
                id: path.to_string(),
                path: path.to_string(),
                key: key.map(String::from),
                value_preview: n.to_string(),
                value_type: JsonValueType::Number,
                depth,
                expandable: false,
                expanded: false,
                raw_value: Some(value.clone()),
            });
        }
        Value::Bool(b) => {
            out.push(JsonTreeNode {
                id: path.to_string(),
                path: path.to_string(),
                key: key.map(String::from),
                value_preview: b.to_string(),
                value_type: JsonValueType::Boolean,
                depth,
                expandable: false,
                expanded: false,
                raw_value: Some(value.clone()),
            });
        }
        Value::Null => {
            out.push(JsonTreeNode {
                id: path.to_string(),
                path: path.to_string(),
                key: key.map(String::from),
                value_preview: "null".to_string(),
                value_type: JsonValueType::Null,
                depth,
                expandable: false,
                expanded: false,
                raw_value: Some(value.clone()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;
    use wakuwaku_protocol::{
        TrajectoryAvailability, TrajectoryDetailSection, TrajectoryKind, TrajectoryLane,
        TrajectoryLiveUpdate, TrajectoryRowSummary, TrajectoryStatus,
    };

    #[allow(clippy::too_many_arguments)]
    fn make_test_record(
        id: Uuid,
        turn_count: u32,
        step: u32,
        kind: TrajectoryKind,
        lane: TrajectoryLane,
        title: &str,
        preview: &str,
        search_text: &str,
        started_at: Option<i64>,
        duration_ms: Option<i64>,
        parent_id: Option<Uuid>,
    ) -> TrajectoryRowSummary {
        TrajectoryRowSummary {
            record_id: id,
            sequence: (turn_count as u64) * 10 + (step as u64),
            revision: 1,
            request_id: None,
            parent_record_id: parent_id,
            prompt_id: None,
            turn_count,
            step,
            kind,
            lane,
            status: TrajectoryStatus::Completed,
            title: title.to_string(),
            preview: preview.to_string(),
            search_text: search_text.to_string(),
            started_at_ms: started_at,
            first_token_at_ms: started_at.map(|s| s + 50),
            completed_at_ms: started_at.and_then(|s| duration_ms.map(|d| s + d)),
            duration_ms,
            ttft_ms: Some(50),
        }
    }

    #[test]
    fn test_and_search_multi_token_case_insensitive() {
        let mut index = HashMap::new();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        index.insert(
            id1,
            "read_file path/to/index.ts executed successfully".to_lowercase(),
        );
        index.insert(
            id2,
            "write_file path/to/output.txt created file".to_lowercase(),
        );

        let tokens1 = vec!["read".to_string()];
        assert!(matches_and_search(id1, &tokens1, &index));
        assert!(!matches_and_search(id2, &tokens1, &index));

        let tokens_multi = vec!["read".to_string(), "index.ts".to_string()];
        assert!(matches_and_search(id1, &tokens_multi, &index));
        assert!(!matches_and_search(id2, &tokens_multi, &index));

        let tokens_nomatch = vec!["read".to_string(), "output.txt".to_string()];
        assert!(!matches_and_search(id1, &tokens_nomatch, &index));

        let tokens_empty = Vec::new();
        assert!(matches_and_search(id1, &tokens_empty, &index));
        assert!(matches_and_search(id2, &tokens_empty, &index));
    }

    #[test]
    fn test_duration_formatting_and_unknown_timing() {
        assert_eq!(format_exact_duration(None), "No timing data");
        assert_eq!(format_exact_duration(Some(0)), "0ms");
        assert_eq!(format_exact_duration(Some(350)), "350ms");
        assert_eq!(format_exact_duration(Some(1200)), "1.20s");
        assert_eq!(format_exact_duration(Some(65000)), "1m 5s");
        assert_eq!(format_exact_duration(Some(3665000)), "1h 1m 5s");
    }

    #[test]
    fn test_lane_mapping_and_parent_request_reuse() {
        let req_id = Uuid::new_v4();
        let ast_id = Uuid::new_v4();

        let req = make_test_record(
            req_id,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "Request",
            "Model call",
            "request",
            Some(1000),
            Some(500),
            None,
        );

        // Assistant with no started_at_ms reuses parent request started_at_ms
        let ast = make_test_record(
            ast_id,
            1,
            1,
            TrajectoryKind::Assistant,
            TrajectoryLane::Model,
            "Assistant",
            "Thought and response",
            "assistant",
            None,
            Some(200),
            Some(req_id),
        );

        let layout = compute_timeline_layout(&[req, ast]);
        assert_eq!(layout.model_spans.len(), 2);
        assert_eq!(layout.model_spans[0].lane, TrajectoryLane::Model);
        assert_eq!(layout.model_spans[1].lane, TrajectoryLane::Model);
        // Both spans should have valid timing mapped
        assert!(layout.model_spans[0].has_timing);
        assert!(layout.model_spans[1].has_timing);
    }

    #[test]
    fn test_grouped_mapping_and_folding() {
        let mut state = TrajectorySessionState::new(Uuid::new_v4());
        let sys_id = Uuid::new_v4();
        let req_id = Uuid::new_v4();
        let tool_id = Uuid::new_v4();
        let ctx_id = Uuid::new_v4();

        let sys = make_test_record(
            sys_id,
            0,
            0,
            TrajectoryKind::System,
            TrajectoryLane::Input,
            "System",
            "System prompt",
            "system",
            Some(100),
            Some(10),
            None,
        );
        let req = make_test_record(
            req_id,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "Step 1",
            "Execute step",
            "step 1",
            Some(200),
            Some(50),
            None,
        );
        let tool = make_test_record(
            tool_id,
            1,
            1,
            TrajectoryKind::Tool,
            TrajectoryLane::Tools,
            "Tool read_file",
            "Read file",
            "tool read",
            Some(260),
            Some(40),
            Some(req_id),
        );
        let ctx = make_test_record(
            ctx_id,
            1,
            2,
            TrajectoryKind::Context,
            TrajectoryLane::Input,
            "Steering",
            "User steer",
            "steer",
            Some(350),
            Some(5),
            None,
        );

        state.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            1,
            vec![sys, req, tool, ctx],
            None,
            None,
            false,
            false,
        );

        assert_eq!(state.loading_status, TrajectoryLoadingStatus::Ready);
        assert_eq!(state.records.len(), 4);
        // Ledger should contain: Turn 0 (System), Turn 1 Divider, Step 1 Request, Tool child (with placeholder if needed), Context Steering
        assert!(!state.ledger_rows.is_empty());

        let turn_divider_exists = state.ledger_rows.iter().any(|r| {
            matches!(
                r.kind,
                TrajectoryLedgerRowKind::TurnDivider { turn_count: 1, .. }
            )
        });
        assert!(turn_divider_exists);

        // Test fold turn
        state.toggle_turn_fold(1);
        assert!(state.folded_turns.contains(&1));
        // When turn 1 is folded, only Turn Divider and non-turn items remain
        let step_visible = state
            .ledger_rows
            .iter()
            .any(|r| matches!(r.kind, TrajectoryLedgerRowKind::StepRequest { .. }));
        assert!(!step_visible);

        // Unfold all turns
        state.unfold_all_turns();
        assert!(!state.folded_turns.contains(&1));
        let step_visible_again = state
            .ledger_rows
            .iter()
            .any(|r| matches!(r.kind, TrajectoryLedgerRowKind::StepRequest { .. }));
        assert!(step_visible_again);

        // Toggle tool calls visibility
        state.toggle_tool_calls();
        assert!(!state.show_tool_calls);
        let tool_visible = state
            .ledger_rows
            .iter()
            .any(|r| matches!(r.kind, TrajectoryLedgerRowKind::Tool { .. }));
        assert!(!tool_visible);
    }

    #[test]
    fn test_prepend_older_page_preserves_anchor() {
        let mut state = TrajectorySessionState::new(Uuid::new_v4());
        let id_b = Uuid::new_v4();
        let id_c = Uuid::new_v4();
        let id_a = Uuid::new_v4();

        let rec_b = make_test_record(
            id_b,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "B",
            "b",
            "b",
            Some(200),
            Some(10),
            None,
        );
        let rec_c = make_test_record(
            id_c,
            1,
            2,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "C",
            "c",
            "c",
            Some(300),
            Some(10),
            None,
        );

        state.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            2,
            vec![rec_b, rec_c],
            Some(TrajectoryCursor {
                sequence: 10,
                record_id: id_b,
            }),
            None,
            true,
            false,
        );

        state.select_record(id_b);
        assert_eq!(state.selected_record_id, Some(id_b));

        // Prepend older record A
        let rec_a = make_test_record(
            id_a,
            1,
            0,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "A",
            "a",
            "a",
            Some(100),
            Some(10),
            None,
        );
        state.prepend_older_page(vec![rec_a], None, false);

        // Verify records contains A, B, C and selected record remains id_b
        let ids: Vec<Uuid> = state.records.iter().map(|r| r.record_id).collect();
        assert_eq!(ids, vec![id_a, id_b, id_c]);
        assert_eq!(state.selected_record_id, Some(id_b));
    }

    #[test]
    fn test_tail_follow_and_live_merging() {
        let mut state = TrajectorySessionState::new(Uuid::new_v4());
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();

        let rec1 = make_test_record(
            id1,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "R1",
            "r1",
            "r1",
            Some(100),
            Some(10),
            None,
        );
        state.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            1,
            vec![rec1],
            None,
            None,
            false,
            false,
        );

        assert_eq!(state.revision, 1);
        assert_eq!(state.records.len(), 1);

        // Live upsert update
        let rec2 = make_test_record(
            id2,
            1,
            2,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "R2",
            "r2",
            "r2",
            Some(200),
            Some(10),
            None,
        );
        state.apply_live_update(TrajectoryLiveUpdate::Upsert {
            generation: 1,
            revision: 2,
            row: Box::new(rec2),
        });

        assert_eq!(state.revision, 2);
        let ids: Vec<Uuid> = state.records.iter().map(|r| r.record_id).collect();
        assert_eq!(ids, vec![id1, id2]);
        assert_eq!(state.records.len(), 2);
    }

    #[test]
    fn test_revision_merge_and_generation_reset() {
        let mut state = TrajectorySessionState::new(Uuid::new_v4());
        let id1 = Uuid::new_v4();
        let rec1 = make_test_record(
            id1,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "R1",
            "r1",
            "r1",
            Some(100),
            Some(10),
            None,
        );

        state.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            5,
            vec![rec1],
            None,
            None,
            false,
            false,
        );
        assert_eq!(state.revision, 5);

        // Stale generation update rejected
        let id2 = Uuid::new_v4();
        let rec2 = make_test_record(
            id2,
            1,
            2,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "R2",
            "r2",
            "r2",
            Some(200),
            Some(10),
            None,
        );
        state.apply_live_update(TrajectoryLiveUpdate::Upsert {
            generation: 0, // Stale generation
            revision: 4,
            row: Box::new(rec2),
        });
        let ids: Vec<Uuid> = state.records.iter().map(|r| r.record_id).collect();
        assert_eq!(ids, vec![id1]);

        // Live Reset clears previous records and resets generation
        state.apply_live_update(TrajectoryLiveUpdate::Reset {
            generation: 2,
            revision: 1,
        });
        assert_eq!(state.generation, 2);
        assert_eq!(state.revision, 1);
        assert!(state.records.is_empty());
    }

    #[test]
    fn test_inspector_tab_mapping_by_record_kind() {
        let sys_tabs = allowed_sections_for_kind(TrajectoryKind::System);
        assert_eq!(
            sys_tabs,
            vec![
                TrajectoryDetailSection::Summary,
                TrajectoryDetailSection::SystemPrompt,
                TrajectoryDetailSection::Source,
                TrajectoryDetailSection::Raw,
            ]
        );

        let req_tabs = allowed_sections_for_kind(TrajectoryKind::Request);
        assert_eq!(
            req_tabs,
            vec![
                TrajectoryDetailSection::Summary,
                TrajectoryDetailSection::SystemPrompt,
                TrajectoryDetailSection::Tools,
                TrajectoryDetailSection::Options,
                TrajectoryDetailSection::Usage,
                TrajectoryDetailSection::Timing,
                TrajectoryDetailSection::Source,
                TrajectoryDetailSection::Raw,
            ]
        );

        let ast_tabs = allowed_sections_for_kind(TrajectoryKind::Assistant);
        assert_eq!(
            ast_tabs,
            vec![
                TrajectoryDetailSection::Summary,
                TrajectoryDetailSection::Preview,
                TrajectoryDetailSection::Usage,
                TrajectoryDetailSection::Timing,
                TrajectoryDetailSection::Source,
                TrajectoryDetailSection::Raw,
            ]
        );

        let tool_tabs = allowed_sections_for_kind(TrajectoryKind::Tool);
        assert_eq!(
            tool_tabs,
            vec![
                TrajectoryDetailSection::Summary,
                TrajectoryDetailSection::Payload,
                TrajectoryDetailSection::Result,
                TrajectoryDetailSection::Diff,
                TrajectoryDetailSection::Schema,
                TrajectoryDetailSection::Timing,
                TrajectoryDetailSection::Source,
                TrajectoryDetailSection::Raw,
            ]
        );

        let ctx_tabs = allowed_sections_for_kind(TrajectoryKind::Context);
        assert_eq!(
            ctx_tabs,
            vec![
                TrajectoryDetailSection::Summary,
                TrajectoryDetailSection::Preview,
                TrajectoryDetailSection::Diff,
                TrajectoryDetailSection::Source,
                TrajectoryDetailSection::Raw,
            ]
        );
    }

    #[test]
    fn test_escape_key_closes_inspector_then_clears_search() {
        let mut state = TrajectorySessionState::new(Uuid::new_v4());
        let id1 = Uuid::new_v4();
        let rec1 = make_test_record(
            id1,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "R1",
            "r1",
            "r1",
            Some(100),
            Some(10),
            None,
        );

        state.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            1,
            vec![rec1],
            None,
            None,
            false,
            false,
        );

        state.set_search_query("test query".to_string());
        state.select_record(id1);

        assert!(state.inspector_open);
        assert_eq!(state.search_query, "test query");

        // First escape: closes inspector
        state.close_inspector();
        assert!(!state.inspector_open);
        assert_eq!(state.search_query, "test query");

        // Second escape: clears search query
        state.set_search_query(String::new());
        assert_eq!(state.search_query, "");
    }

    #[test]
    fn test_json_tree_flattening_and_virtualization() {
        let sample_json = json!({
            "model": "claude-3-5-sonnet",
            "config": {
                "temperature": 0.7,
                "max_tokens": 4096,
                "tools": ["read_file", "write_file"]
            },
            "status": "success"
        });

        let mut expanded = HashSet::new();
        expanded.insert("$".to_string());
        expanded.insert("$.config".to_string());
        let nodes = flatten_json_tree(&sample_json, &expanded);

        // Root and depth-1 should be flattened and visible
        assert!(!nodes.is_empty());
        let root_node = &nodes[0];
        assert_eq!(root_node.depth, 0);

        // Toggle expand/collapse on config node
        expanded.remove("$.config");
        let collapsed_nodes = flatten_json_tree(&sample_json, &expanded);
        assert!(collapsed_nodes.len() < nodes.len());
    }

    #[test]
    fn test_row_builder_o_visible_evidence() {
        let mut state = TrajectorySessionState::new(Uuid::new_v4());
        let id1 = Uuid::new_v4();
        let rec1 = make_test_record(
            id1,
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "Step 1",
            "Execute prompt",
            "step prompt",
            Some(100),
            Some(50),
            None,
        );

        state.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            1,
            vec![rec1],
            None,
            None,
            false,
            false,
        );

        // Verify ledger rows have precalculated metadata ready for O(1) rendering
        for row in &state.ledger_rows {
            assert!(!row.key.is_empty());
            match &row.kind {
                TrajectoryLedgerRowKind::TurnDivider { turn_count, .. } => {
                    assert_eq!(*turn_count, 1);
                }
                TrajectoryLedgerRowKind::StepRequest { record, .. } => {
                    assert_eq!(record.status, TrajectoryStatus::Completed);
                    assert_eq!(record.kind, TrajectoryKind::Request);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn test_trajectory_per_session_state_and_chat_isolation() {
        let session_a = Uuid::new_v4();
        let session_b = Uuid::new_v4();

        let mut trajectory_sessions: HashMap<Uuid, TrajectorySessionState> = HashMap::new();

        // Session A initializes trajectory state with records
        let mut state_a = TrajectorySessionState::new(session_a);
        let rec_a = make_test_record(
            Uuid::new_v4(),
            1,
            1,
            TrajectoryKind::Request,
            TrajectoryLane::Model,
            "Session A Step",
            "Preview A",
            "Search A",
            Some(100),
            Some(50),
            None,
        );
        state_a.set_page_response(
            TrajectoryAvailability::Exact,
            1,
            1,
            vec![rec_a.clone()],
            None,
            None,
            false,
            false,
        );
        state_a.select_record(rec_a.record_id);
        assert!(state_a.inspector_open);
        assert_eq!(state_a.selected_record_id, Some(rec_a.record_id));
        trajectory_sessions.insert(session_a, state_a);

        // Session B initializes trajectory state independently
        let state_b = TrajectorySessionState::new(session_b);
        assert!(!state_b.inspector_open);
        assert_eq!(state_b.selected_record_id, None);
        assert!(state_b.records.is_empty());
        trajectory_sessions.insert(session_b, state_b);

        // Mutating Session A's trajectory does not mutate Session B
        let a = trajectory_sessions.get_mut(&session_a).unwrap();
        a.close_inspector();
        assert!(!a.inspector_open);
        assert_eq!(a.selected_record_id, Some(rec_a.record_id)); // row focus preserved

        let b = trajectory_sessions.get(&session_b).unwrap();
        assert!(!b.inspector_open);
        assert_eq!(b.selected_record_id, None);
        assert!(b.records.is_empty());

        // Removing Session A only clears Session A from map
        trajectory_sessions.remove(&session_a);
        assert!(!trajectory_sessions.contains_key(&session_a));
        assert!(trajectory_sessions.contains_key(&session_b));
    }
}
