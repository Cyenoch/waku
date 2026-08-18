use crossbeam_channel::{Receiver, unbounded};
use uuid::Uuid;
use wakuwaku_protocol::{
    Command, ResponsePayload, SequencedPayload, TrajectoryDetailSection, TrajectoryLiveUpdate,
    TrajectoryQuery, TrajectoryResponse,
};

use crate::DaemonClient;

/// Typed page/detail/live wrapper around daemon-owned trajectory RPCs.
#[derive(Clone)]
pub struct TrajectoryClient {
    client: DaemonClient,
}

impl TrajectoryClient {
    pub fn new(client: DaemonClient) -> Self {
        Self { client }
    }

    pub fn query(
        &self,
        session_id: Uuid,
        query: TrajectoryQuery,
    ) -> anyhow::Result<TrajectoryResponse> {
        expect_trajectory(self.client.request(
            session_id,
            Uuid::nil(),
            Command::QueryTrajectory { query },
        )?)
    }

    pub fn page(
        &self,
        session_id: Uuid,
        before: Option<wakuwaku_protocol::TrajectoryCursor>,
        limit: Option<u32>,
        at_least_revision: Option<u64>,
    ) -> anyhow::Result<TrajectoryResponse> {
        self.query(
            session_id,
            TrajectoryQuery::Page {
                before,
                limit,
                at_least_revision,
            },
        )
    }

    pub fn detail(
        &self,
        session_id: Uuid,
        record_id: Uuid,
        section: TrajectoryDetailSection,
        cursor: Option<u64>,
        limit: Option<u32>,
        at_least_revision: Option<u64>,
    ) -> anyhow::Result<TrajectoryResponse> {
        self.query(
            session_id,
            TrajectoryQuery::Detail {
                record_id,
                section,
                cursor,
                limit,
                at_least_revision,
            },
        )
    }

    /// Filters Hub events for committed trajectory updates on one stream.
    pub fn subscribe(&self, session_id: Uuid, runtime_id: Uuid) -> Receiver<TrajectoryLiveUpdate> {
        let sequenced = self.client.subscribe(session_id, runtime_id);
        let (tx, rx) = unbounded();
        let _ = std::thread::Builder::new()
            .name(format!("wakuwaku-trajectory-live-{session_id}"))
            .spawn(move || {
                while let Ok(event) = sequenced.recv() {
                    if let SequencedPayload::Trajectory { update } = event.payload
                        && tx.send(update).is_err()
                    {
                        break;
                    }
                }
            });
        rx
    }
}

fn expect_trajectory(payload: ResponsePayload) -> anyhow::Result<TrajectoryResponse> {
    match payload {
        ResponsePayload::Trajectory { response } => Ok(*response),
        _ => anyhow::bail!("WakuWaku daemon returned an invalid trajectory response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wakuwaku_protocol::{TrajectoryAvailability, TrajectoryLiveUpdate};

    #[test]
    fn unexpected_payload_is_a_typed_error() {
        let error = expect_trajectory(ResponsePayload::Ack).unwrap_err();
        assert!(error.to_string().contains("invalid trajectory response"));
    }

    #[test]
    fn trajectory_payload_is_unboxed() {
        let response = expect_trajectory(ResponsePayload::Trajectory {
            response: Box::new(TrajectoryResponse::Page {
                availability: TrajectoryAvailability::Exact,
                generation: 1,
                revision: 2,
                rows: Vec::new(),
                older: None,
                newer: None,
                has_older: false,
                has_newer: false,
            }),
        })
        .unwrap();
        assert!(matches!(
            response,
            TrajectoryResponse::Page { revision: 2, .. }
        ));
    }

    #[test]
    fn live_update_variant_is_independent_of_transport_sequence() {
        let update = TrajectoryLiveUpdate::Reset {
            generation: 4,
            revision: 11,
        };
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["type"], "reset");
        assert_eq!(json["revision"], 11);
        assert!(json.get("sequence").is_none());
    }
}
