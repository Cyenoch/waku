use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

/// Default page size when a query omits `limit`.
pub const TRAJECTORY_PAGE_DEFAULT: u32 = 100;
/// Daemon clamp for page and detail row limits.
pub const TRAJECTORY_PAGE_MAX: u32 = 200;
/// UTF-8-safe byte window for large detail strings.
pub const TRAJECTORY_DETAIL_WINDOW_BYTES: u32 = 64 * 1024;
/// Live/search source text character bound.
pub const TRAJECTORY_SEARCH_SOURCE_CHARS: usize = 2048;
/// Live/search output text character bound.
pub const TRAJECTORY_SEARCH_OUTPUT_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryCursor {
    pub sequence: u64,
    pub record_id: Uuid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TrajectoryKind {
    System,
    User,
    Context,
    Request,
    Assistant,
    Tool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TrajectoryLane {
    Input,
    Model,
    Tools,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TrajectoryStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TrajectoryAvailability {
    Exact,
    Legacy,
    LegacyPartialMissingSnapshot,
    Unavailable,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub enum TrajectoryDetailSection {
    Summary,
    Preview,
    Raw,
    Source,
    SystemPrompt,
    Tools,
    Diff,
    Options,
    Usage,
    Timing,
    Payload,
    Result,
    Schema,
}

#[derive(Clone, Debug, Deserialize, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TrajectoryQuery {
    Page {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<TrajectoryCursor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least_revision: Option<u64>,
    },
    Detail {
        record_id: Uuid,
        section: TrajectoryDetailSection,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_least_revision: Option<u64>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryRowSummary {
    pub record_id: Uuid,
    pub sequence: u64,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_record_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<Uuid>,
    pub turn_count: u32,
    pub step: u32,
    pub kind: TrajectoryKind,
    pub lane: TrajectoryLane,
    pub status: TrajectoryStatus,
    pub title: String,
    pub preview: String,
    pub search_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct TrajectoryDetailContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub offset: u64,
    pub byte_length: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TrajectoryResponse {
    Page {
        availability: TrajectoryAvailability,
        generation: u64,
        revision: u64,
        rows: Vec<TrajectoryRowSummary>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        older: Option<TrajectoryCursor>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        newer: Option<TrajectoryCursor>,
        has_older: bool,
        has_newer: bool,
    },
    Detail {
        record_id: Uuid,
        section: TrajectoryDetailSection,
        generation: u64,
        revision: u64,
        content: TrajectoryDetailContent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<u64>,
        has_more: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, TS)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TrajectoryLiveUpdate {
    Upsert {
        generation: u64,
        revision: u64,
        row: TrajectoryRowSummary,
    },
    Remove {
        generation: u64,
        revision: u64,
        record_id: Uuid,
    },
    Reset {
        generation: u64,
        revision: u64,
    },
}

pub fn clamp_trajectory_page_limit(limit: Option<u32>) -> u32 {
    limit
        .unwrap_or(TRAJECTORY_PAGE_DEFAULT)
        .clamp(1, TRAJECTORY_PAGE_MAX)
}

pub fn clamp_trajectory_detail_window(limit: Option<u32>) -> u32 {
    limit
        .unwrap_or(TRAJECTORY_DETAIL_WINDOW_BYTES)
        .clamp(1, TRAJECTORY_DETAIL_WINDOW_BYTES)
}

pub fn bound_search_text(source: &str, output: &str) -> String {
    let mut text: String = source
        .chars()
        .take(TRAJECTORY_SEARCH_SOURCE_CHARS)
        .collect();
    if !output.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.extend(output.chars().take(TRAJECTORY_SEARCH_OUTPUT_CHARS));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::{PromptAttachmentSource, PromptInput};
    use serde_json::json;

    fn sample_row() -> TrajectoryRowSummary {
        TrajectoryRowSummary {
            record_id: Uuid::from_u128(9),
            sequence: 3,
            revision: 4,
            request_id: Some(Uuid::from_u128(8)),
            parent_record_id: None,
            prompt_id: Some(Uuid::from_u128(7)),
            turn_count: 1,
            step: 2,
            kind: TrajectoryKind::Request,
            lane: TrajectoryLane::Model,
            status: TrajectoryStatus::Running,
            title: "Request".into(),
            preview: "hello".into(),
            search_text: "hello".into(),
            started_at_ms: Some(10),
            first_token_at_ms: Some(12),
            completed_at_ms: None,
            duration_ms: None,
            ttft_ms: Some(2),
        }
    }

    #[test]
    fn page_query_round_trips_camel_case() {
        let query = TrajectoryQuery::Page {
            before: Some(TrajectoryCursor {
                sequence: 11,
                record_id: Uuid::from_u128(5),
            }),
            limit: Some(40),
            at_least_revision: Some(6),
        };
        let json = serde_json::to_value(&query).unwrap();
        assert_eq!(json["type"], "page");
        assert_eq!(json["before"]["sequence"], 11);
        assert_eq!(json["before"]["recordId"], Uuid::from_u128(5).to_string());
        assert_eq!(json["atLeastRevision"], 6);
        let back: TrajectoryQuery = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            TrajectoryQuery::Page {
                limit: Some(40),
                at_least_revision: Some(6),
                ..
            }
        ));
    }

    #[test]
    fn detail_query_round_trips_camel_case() {
        let query = TrajectoryQuery::Detail {
            record_id: Uuid::from_u128(3),
            section: TrajectoryDetailSection::SystemPrompt,
            cursor: Some(64),
            limit: Some(1024),
            at_least_revision: Some(2),
        };
        let json = serde_json::to_value(&query).unwrap();
        assert_eq!(json["type"], "detail");
        assert_eq!(json["section"], "systemPrompt");
        assert_eq!(json["recordId"], Uuid::from_u128(3).to_string());
        let back: TrajectoryQuery = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            TrajectoryQuery::Detail {
                section: TrajectoryDetailSection::SystemPrompt,
                cursor: Some(64),
                ..
            }
        ));
    }

    #[test]
    fn page_and_detail_responses_round_trip() {
        let page = TrajectoryResponse::Page {
            availability: TrajectoryAvailability::LegacyPartialMissingSnapshot,
            generation: 2,
            revision: 9,
            rows: vec![sample_row()],
            older: Some(TrajectoryCursor {
                sequence: 1,
                record_id: Uuid::from_u128(1),
            }),
            newer: None,
            has_older: true,
            has_newer: false,
        };
        let json = serde_json::to_value(&page).unwrap();
        assert_eq!(json["type"], "page");
        assert_eq!(json["availability"], "legacyPartialMissingSnapshot");
        assert_eq!(json["rows"][0]["kind"], "request");
        assert_eq!(json["rows"][0]["lane"], "model");
        assert_eq!(json["hasOlder"], true);
        let back: TrajectoryResponse = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            TrajectoryResponse::Page {
                availability: TrajectoryAvailability::LegacyPartialMissingSnapshot,
                has_older: true,
                ..
            }
        ));

        let detail = TrajectoryResponse::Detail {
            record_id: Uuid::from_u128(3),
            section: TrajectoryDetailSection::Source,
            generation: 1,
            revision: 4,
            content: TrajectoryDetailContent {
                json: Some(json!({ "kind": "user" })),
                text: None,
                offset: 0,
                byte_length: 16,
                total_bytes: 16,
            },
            next_cursor: None,
            has_more: false,
        };
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["type"], "detail");
        assert_eq!(json["section"], "source");
        assert_eq!(json["content"]["json"]["kind"], "user");
        let back: TrajectoryResponse = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            TrajectoryResponse::Detail {
                section: TrajectoryDetailSection::Source,
                has_more: false,
                ..
            }
        ));
    }

    #[test]
    fn live_updates_round_trip_without_raw_payload() {
        let upsert = TrajectoryLiveUpdate::Upsert {
            generation: 1,
            revision: 8,
            row: sample_row(),
        };
        let json = serde_json::to_value(&upsert).unwrap();
        assert_eq!(json["type"], "upsert");
        assert_eq!(json["revision"], 8);
        assert!(json["row"].get("detailJson").is_none());
        assert!(json["row"].get("raw").is_none());
        let back: TrajectoryLiveUpdate = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back,
            TrajectoryLiveUpdate::Upsert { revision: 8, .. }
        ));

        let remove = TrajectoryLiveUpdate::Remove {
            generation: 1,
            revision: 9,
            record_id: Uuid::from_u128(4),
        };
        let json = serde_json::to_value(&remove).unwrap();
        assert_eq!(json["type"], "remove");
        assert_eq!(json["recordId"], Uuid::from_u128(4).to_string());

        let reset = TrajectoryLiveUpdate::Reset {
            generation: 3,
            revision: 1,
        };
        let json = serde_json::to_value(&reset).unwrap();
        assert_eq!(json["type"], "reset");
        assert_eq!(json["generation"], 3);
    }

    #[test]
    fn prompt_input_extension_round_trips() {
        let input = PromptInput {
            text: "see @notes.md".into(),
            display_text: Some("see".into()),
            attachments: Vec::new(),
            sources: vec![PromptAttachmentSource {
                reference: Some("wakuwaku-blob:notes.md".into()),
                mention: "@notes.md".into(),
                name: "notes.md".into(),
                is_dir: false,
                is_image: false,
                mime: Some("text/markdown".into()),
            }],
        };
        let json = serde_json::to_value(&input).unwrap();
        assert_eq!(json["displayText"], "see");
        assert_eq!(json["sources"][0]["mention"], "@notes.md");
        assert_eq!(json["sources"][0]["isDir"], false);
        let back: PromptInput = serde_json::from_value(json).unwrap();
        assert_eq!(back, input);
    }

    #[test]
    fn page_and_detail_limits_clamp() {
        assert_eq!(clamp_trajectory_page_limit(None), 100);
        assert_eq!(clamp_trajectory_page_limit(Some(0)), 1);
        assert_eq!(clamp_trajectory_page_limit(Some(200)), 200);
        assert_eq!(clamp_trajectory_page_limit(Some(201)), 200);
        assert_eq!(
            clamp_trajectory_detail_window(None),
            TRAJECTORY_DETAIL_WINDOW_BYTES
        );
        assert_eq!(clamp_trajectory_detail_window(Some(0)), 1);
        assert_eq!(
            clamp_trajectory_detail_window(Some(TRAJECTORY_DETAIL_WINDOW_BYTES + 8)),
            TRAJECTORY_DETAIL_WINDOW_BYTES
        );
    }

    #[test]
    fn search_text_is_bounded() {
        let source = "s".repeat(TRAJECTORY_SEARCH_SOURCE_CHARS + 40);
        let output = "o".repeat(TRAJECTORY_SEARCH_OUTPUT_CHARS + 40);
        let bound = bound_search_text(&source, &output);
        assert_eq!(
            bound.chars().count(),
            TRAJECTORY_SEARCH_SOURCE_CHARS + 1 + TRAJECTORY_SEARCH_OUTPUT_CHARS
        );
    }
}
