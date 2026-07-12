//! `FakeAgent` — an in-process tonic server implementing the `doover.DeviceAgent`
//! service for integration tests (the Rust analogue of pydoover's
//! `MockDeviceAgentInterface`, but exercising the real gRPC transport).
//!
//! Behavior: aggregates live in an in-memory map (merge-patch on update,
//! 404 on missing channel), every write is recorded for assertions, and
//! event-subscription streams are backed by mpsc channels the test drives
//! explicitly via [`FakeAgentState::publish_event`] — the fake does not echo
//! its own writes back as events, so tests stay deterministic.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use doover_proto::device_agent as pb;
use pb::device_agent_server::{DeviceAgent, DeviceAgentServer};

// Recorded-call fields exist for assertions; not every test reads every one.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RecordedAggregateWrite {
    pub channel: String,
    pub data: Value,
    pub max_age_secs: f32,
    pub save_log: bool,
    pub replace_data: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RecordedMessage {
    pub channel: String,
    pub data: Value,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RecordedMessageUpdate {
    pub channel: String,
    pub message_id: u64,
    pub data: Value,
    pub replace_data: bool,
}

type EventSender = mpsc::Sender<Result<pb::ChannelEventSubscriptionResponse, Status>>;

#[derive(Default)]
pub struct FakeAgentState {
    pub aggregates: Mutex<HashMap<String, Value>>,
    pub aggregate_writes: Mutex<Vec<RecordedAggregateWrite>>,
    pub messages: Mutex<Vec<RecordedMessage>>,
    pub message_updates: Mutex<Vec<RecordedMessageUpdate>>,
    pub oneshots: Mutex<Vec<RecordedMessage>>,
    event_txs: Mutex<HashMap<String, Vec<EventSender>>>,
    next_message_id: AtomicU64,
}

// Driver helpers exist for whichever test target includes this module; not
// every target calls every one (dead-code is analysed per test binary).
#[allow(dead_code)]
impl FakeAgentState {
    /// Pre-seed a channel aggregate before the client connects.
    pub fn seed_aggregate(&self, channel: &str, data: Value) {
        self.aggregates.lock().unwrap().insert(channel.to_string(), data);
    }

    /// How many event-subscription streams have been opened for a channel
    /// over the fake's lifetime (reconnects open new ones).
    pub fn stream_count(&self, channel: &str) -> usize {
        self.event_txs.lock().unwrap().get(channel).map_or(0, Vec::len)
    }

    /// Drop every live event stream (simulates the agent restarting).
    pub fn drop_streams(&self) {
        self.event_txs.lock().unwrap().clear();
    }

    /// Push an `AggregateUpdate` event to a channel's live subscribers,
    /// also updating the stored aggregate. `diff` rides in `request_data`.
    pub async fn publish_aggregate_update(&self, channel: &str, data: Value, diff: Value) {
        self.aggregates.lock().unwrap().insert(channel.to_string(), data.clone());
        let payload = json!({
            "author_id": 1,
            "channel": {"agent_id": 1, "name": channel},
            "aggregate": {"data": data},
            "request_data": {"data": diff},
            "organisation_id": 1,
        });
        self.publish_event(channel, "AggregateUpdate", payload).await;
    }

    /// Push a raw event to a channel's live subscribers.
    pub async fn publish_event(&self, channel: &str, event_name: &str, payload: Value) {
        let response = pb::ChannelEventSubscriptionResponse {
            response_header: Some(ok_header()),
            event_name: event_name.to_string(),
            channel_name: channel.to_string(),
            data: None,
            data_json: payload.to_string(),
        };
        let senders: Vec<_> = self
            .event_txs
            .lock()
            .unwrap()
            .get(channel)
            .map(|v| v.to_vec())
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.send(Ok(response.clone())).await;
        }
    }
}

fn ok_header() -> pb::ResponseHeader {
    pb::ResponseHeader {
        success: true,
        cloud_synced: true,
        cloud_ready: true,
        response_code: Some(200),
        response_message: None,
    }
}

fn not_found_header() -> pb::ResponseHeader {
    pb::ResponseHeader {
        success: false,
        cloud_synced: true,
        cloud_ready: true,
        response_code: Some(404),
        response_message: Some("channel not found".to_string()),
    }
}

/// Merge-patch `diff` into `base` the way the real agent merges aggregate
/// writes (objects merge recursively, anything else replaces).
fn merge(base: &mut Value, diff: &Value) {
    match (base.as_object_mut(), diff.as_object()) {
        (Some(base_map), Some(diff_map)) => {
            for (k, v) in diff_map {
                match base_map.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
                    _ => {
                        base_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => *base = diff.clone(),
    }
}

pub struct FakeAgent(pub Arc<FakeAgentState>);

#[tonic::async_trait]
impl DeviceAgent for FakeAgent {
    async fn test_comms(
        &self,
        request: Request<pb::TestCommsRequest>,
    ) -> Result<Response<pb::TestCommsResponse>, Status> {
        Ok(Response::new(pb::TestCommsResponse {
            response_header: Some(ok_header()),
            response: request.into_inner().message,
        }))
    }

    async fn get_aggregate(
        &self,
        request: Request<pb::GetAggregateRequest>,
    ) -> Result<Response<pb::GetAggregateResponse>, Status> {
        let channel = request.into_inner().channel_name;
        let aggregates = self.0.aggregates.lock().unwrap();
        let resp = match aggregates.get(&channel) {
            Some(data) => pb::GetAggregateResponse {
                response_header: Some(ok_header()),
                aggregate: Some(pb::Aggregate {
                    data: None,
                    attachments: vec![],
                    last_updated: None,
                    data_json: data.to_string(),
                }),
            },
            None => pb::GetAggregateResponse {
                response_header: Some(not_found_header()),
                aggregate: None,
            },
        };
        Ok(Response::new(resp))
    }

    async fn update_aggregate(
        &self,
        request: Request<pb::UpdateAggregateRequest>,
    ) -> Result<Response<pb::UpdateAggregateResponse>, Status> {
        let req = request.into_inner();
        let data: Value = serde_json::from_str(&req.data_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let merged = {
            let mut aggregates = self.0.aggregates.lock().unwrap();
            let entry = aggregates
                .entry(req.channel_name.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if req.replace_data.unwrap_or(false) {
                *entry = data.clone();
            } else {
                merge(entry, &data);
            }
            entry.clone()
        };
        self.0.aggregate_writes.lock().unwrap().push(RecordedAggregateWrite {
            channel: req.channel_name,
            data,
            max_age_secs: req.max_age_secs,
            save_log: req.save_log,
            replace_data: req.replace_data.unwrap_or(false),
        });
        Ok(Response::new(pb::UpdateAggregateResponse {
            response_header: Some(ok_header()),
            aggregate: Some(pb::Aggregate {
                data: None,
                attachments: vec![],
                last_updated: None,
                data_json: merged.to_string(),
            }),
        }))
    }

    async fn create_message(
        &self,
        request: Request<pb::CreateMessageRequest>,
    ) -> Result<Response<pb::CreateMessageResponse>, Status> {
        let req = request.into_inner();
        let data: Value = serde_json::from_str(&req.data_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.0.messages.lock().unwrap().push(RecordedMessage {
            channel: req.channel_name,
            data,
            timestamp: req.timestamp,
        });
        Ok(Response::new(pb::CreateMessageResponse {
            response_header: Some(ok_header()),
            message_id: self.0.next_message_id.fetch_add(1, Ordering::Relaxed) + 1,
        }))
    }

    async fn send_one_shot_message(
        &self,
        request: Request<pb::SendOneShotMessageRequest>,
    ) -> Result<Response<pb::SendOneShotMessageResponse>, Status> {
        let req = request.into_inner();
        let data: Value = serde_json::from_str(&req.data_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.0.oneshots.lock().unwrap().push(RecordedMessage {
            channel: req.channel_name,
            data,
            timestamp: req.timestamp.unwrap_or(0),
        });
        Ok(Response::new(pb::SendOneShotMessageResponse {
            response_header: Some(ok_header()),
        }))
    }

    type ChannelEventSubscriptionStream =
        ReceiverStream<Result<pb::ChannelEventSubscriptionResponse, Status>>;

    async fn channel_event_subscription(
        &self,
        request: Request<pb::ChannelEventSubscriptionRequest>,
    ) -> Result<Response<Self::ChannelEventSubscriptionStream>, Status> {
        let channel = request.into_inner().channel_name;
        let (tx, rx) = mpsc::channel(64);
        self.0.event_txs.lock().unwrap().entry(channel).or_default().push(tx);
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type GetChannelSubscriptionStream =
        ReceiverStream<Result<pb::ChannelSubscriptionResponse, Status>>;

    async fn get_channel_subscription(
        &self,
        _request: Request<pb::ChannelSubscriptionRequest>,
    ) -> Result<Response<Self::GetChannelSubscriptionStream>, Status> {
        Err(Status::unimplemented("GetChannelSubscription"))
    }

    async fn write_to_channel(
        &self,
        _request: Request<pb::ChannelWriteRequest>,
    ) -> Result<Response<pb::ChannelWriteResponse>, Status> {
        Err(Status::unimplemented("WriteToChannel"))
    }

    async fn get_debug_info(
        &self,
        _request: Request<pb::DebugInfoRequest>,
    ) -> Result<Response<pb::DebugInfoResponse>, Status> {
        Err(Status::unimplemented("GetDebugInfo"))
    }

    async fn get_turn_credential(
        &self,
        _request: Request<pb::TurnCredentialRequest>,
    ) -> Result<Response<pb::TurnCredentialResponse>, Status> {
        Err(Status::unimplemented("GetTurnCredential"))
    }

    async fn update_message(
        &self,
        request: Request<pb::UpdateMessageRequest>,
    ) -> Result<Response<pb::UpdateMessageResponse>, Status> {
        let req = request.into_inner();
        let data: Value = serde_json::from_str(&req.data_json)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let message_id: u64 = req
            .message_id
            .parse()
            .map_err(|e| Status::invalid_argument(format!("bad message_id: {e}")))?;
        self.0.message_updates.lock().unwrap().push(RecordedMessageUpdate {
            channel: req.channel_name.clone(),
            message_id,
            data: data.clone(),
            replace_data: req.replace_data.unwrap_or(false),
        });
        Ok(Response::new(pb::UpdateMessageResponse {
            response_header: Some(ok_header()),
            message: Some(pb::Message {
                message_id,
                author_id: 0,
                channel: Some(pb::ChannelId { agent_id: 0, name: req.channel_name }),
                data: None,
                attachments: vec![],
                data_json: data.to_string(),
            }),
        }))
    }

    async fn get_message(
        &self,
        _request: Request<pb::GetMessageRequest>,
    ) -> Result<Response<pb::GetMessageResponse>, Status> {
        Err(Status::unimplemented("GetMessage"))
    }

    async fn get_messages(
        &self,
        _request: Request<pb::GetMessagesRequest>,
    ) -> Result<Response<pb::GetMessagesResponse>, Status> {
        Err(Status::unimplemented("GetMessages"))
    }

    async fn fetch_attachment(
        &self,
        _request: Request<pb::FetchAttachmentRequest>,
    ) -> Result<Response<pb::FetchAttachmentResponse>, Status> {
        Err(Status::unimplemented("FetchAttachment"))
    }
}

/// Start a fake agent on an ephemeral local port; returns the shared state
/// and a `http://…` URI for `DeviceAgentClient::connect`.
pub async fn spawn_fake_agent() -> (Arc<FakeAgentState>, String) {
    let state = Arc::new(FakeAgentState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind fake agent");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let service = DeviceAgentServer::new(FakeAgent(state.clone()));
    tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await;
    });
    (state, format!("http://{addr}"))
}
