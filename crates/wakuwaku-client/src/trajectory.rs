use crossbeam_channel::Receiver;
use uuid::Uuid;
use wakuwaku_protocol::{
    TrajectoryCursor, TrajectoryDetailSection, TrajectoryLiveUpdate,
    TrajectoryResponse,
};

use crate::DaemonClient;

#[derive(Clone)]
pub struct TrajectoryClient {
    #[allow(dead_code)]
    client: DaemonClient,
}

impl TrajectoryClient {
    pub fn new(client: DaemonClient) -> Self {
        Self { client }
    }

    pub fn subscribe(&self, _session_id: Uuid, _runtime_id: Uuid) -> Receiver<TrajectoryLiveUpdate> {
        let (_tx, rx) = crossbeam_channel::unbounded();
        rx
    }

    pub fn page(
        &self,
        _session_id: Uuid,
        _before: Option<TrajectoryCursor>,
        _limit: Option<usize>,
        _after: Option<TrajectoryCursor>,
    ) -> anyhow::Result<TrajectoryResponse> {
        Ok(TrajectoryResponse::Page {
            availability: wakuwaku_protocol::TrajectoryAvailability::Exact,
            generation: 0,
            revision: 0,
            rows: Vec::new(),
            older: None,
            newer: None,
            has_older: false,
            has_newer: false,
        })
    }

    pub fn detail(
        &self,
        _session_id: Uuid,
        record_id: Uuid,
        section: TrajectoryDetailSection,
        cursor: Option<u64>,
        _limit_bytes: Option<usize>,
        _tail: Option<bool>,
    ) -> anyhow::Result<TrajectoryResponse> {
        Ok(TrajectoryResponse::Detail {
            record_id,
            section,
            cursor: cursor.unwrap_or(0),
            content: wakuwaku_protocol::TrajectoryDetailContent::default(),
            next_cursor: None,
            total_bytes: 0,
        })
    }
}
