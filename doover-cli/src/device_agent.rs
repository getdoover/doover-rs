//! `doover device_agent <cmd>` — mirrors the `@cli_command`-decorated surface
//! of pydoover's `DeviceAgentInterface` (`docker/device_agent/device_agent.py`).

use std::time::Duration;

use clap::Subcommand;
use futures_util::StreamExt;
use serde_json::{json, Value};

use doover::docker::device_agent::{
    AggregateOptions, DeviceAgentClient, ListMessagesOptions, Message, UpdateMessageOptions,
};

use crate::{normalize_uri, parse, print_json, CliResult};

/// pydoover's default comms-check message.
const DEFAULT_COMMS_MESSAGE: &str = "Comms Check Message";

#[derive(Subcommand, Debug)]
pub enum DeviceAgentCmd {
    /// Liveness echo against the device agent.
    #[command(name = "test_comms", alias = "test-comms")]
    TestComms {
        /// Message for the agent to echo back.
        #[arg(long, default_value = DEFAULT_COMMS_MESSAGE)]
        message: String,
    },

    /// Whether the device agent answered the last request.
    #[command(name = "get_is_dda_available", alias = "get-is-dda-available")]
    GetIsDdaAvailable,

    /// Whether the device agent reports being synced with the cloud.
    #[command(name = "get_is_dda_online", alias = "get-is-dda-online")]
    GetIsDdaOnline,

    /// Whether the device agent has reported cloud sync at least once.
    #[command(name = "get_has_dda_been_online", alias = "get-has-dda-been-online")]
    GetHasDdaBeenOnline,

    /// Fetch a channel's current aggregate payload.
    #[command(
        name = "fetch_channel_aggregate",
        aliases = ["fetch-channel-aggregate", "get_aggregate", "get-aggregate"]
    )]
    FetchChannelAggregate {
        /// Name of channel to get the aggregate from.
        channel_name: String,
    },

    /// Merge (or replace) a JSON payload into a channel's aggregate.
    #[command(
        name = "update_channel_aggregate",
        aliases = ["update-channel-aggregate", "update_aggregate", "update-aggregate"]
    )]
    UpdateChannelAggregate {
        /// Name of channel to update.
        channel_name: String,
        /// Inline JSON object to merge, e.g. '{"level": 42}'.
        #[arg(value_parser = parse::parse_json)]
        data: Value,
        /// Replace the whole aggregate rather than merge-patching it.
        #[arg(long = "replace_data", alias = "replace-data")]
        replace_data: bool,
        /// Clear the aggregate's attachments.
        #[arg(long = "clear_attachments", alias = "clear-attachments")]
        clear_attachments: bool,
        /// Log a historical datapoint in addition to updating current state.
        #[arg(long = "save_log", alias = "save-log")]
        save_log: bool,
        /// Coalesce with other writes to this channel and flush at most this
        /// old (seconds). 0 publishes immediately.
        #[arg(long = "max_age_secs", alias = "max-age-secs", default_value_t = 0.0)]
        max_age_secs: f32,
    },

    /// Append a message to a channel log; prints the minted message id.
    #[command(name = "create_message", alias = "create-message")]
    CreateMessage {
        /// Name of channel to post the message to.
        channel_name: String,
        /// Inline JSON object for the message body, e.g. '{"level": 42}'.
        #[arg(value_parser = parse::parse_json)]
        data: Value,
        /// Unix-millisecond timestamp to stamp the message with (default: now).
        #[arg(long)]
        timestamp: Option<u64>,
    },

    /// Send an ephemeral one-shot message (requires the agent to be online).
    #[command(
        name = "send_oneshot_message",
        aliases = ["send-oneshot-message", "send_oneshot", "send-oneshot"]
    )]
    SendOneshotMessage {
        /// Name of channel to send the one-shot on.
        channel_name: String,
        /// Inline JSON object for the message body.
        #[arg(value_parser = parse::parse_json)]
        data: Value,
    },

    /// Fetch a single message by id.
    #[command(name = "fetch_message", alias = "fetch-message")]
    FetchMessage {
        /// Name of channel the message lives on.
        channel_name: String,
        /// Snowflake id of the message.
        message_id: u64,
    },

    /// List messages on a channel, bounded by snowflake ids.
    #[command(name = "list_messages", alias = "list-messages")]
    ListMessages {
        /// Name of channel to list messages from.
        channel_name: String,
        /// Only messages with an id below this snowflake.
        #[arg(long)]
        before: Option<u64>,
        /// Only messages with an id above this snowflake.
        #[arg(long)]
        after: Option<u64>,
        /// Maximum number of messages to return.
        #[arg(long)]
        limit: Option<u32>,
        /// Comma-separated top-level fields to include in the payloads.
        #[arg(long = "field_names", alias = "field-names", value_delimiter = ',')]
        field_names: Vec<String>,
    },

    /// Update an existing message's payload.
    #[command(name = "update_message", alias = "update-message")]
    UpdateMessage {
        /// Name of channel the message lives on.
        channel_name: String,
        /// Snowflake id of the message.
        message_id: u64,
        /// Inline JSON object to merge into the message body.
        #[arg(value_parser = parse::parse_json)]
        data: Value,
        /// Replace the whole payload rather than merge-patching it.
        #[arg(long = "replace_data", alias = "replace-data")]
        replace_data: bool,
        /// Clear the message's attachments.
        #[arg(long = "clear_attachments", alias = "clear-attachments")]
        clear_attachments: bool,
    },

    /// Fetch WebRTC TURN credentials for camera streaming.
    #[command(name = "fetch_turn_token", alias = "fetch-turn-token")]
    FetchTurnToken {
        /// Camera name to request credentials for.
        #[arg(long = "camera_name", alias = "camera-name", default_value = "")]
        camera_name: String,
    },

    /// Listen to channel events, printing one JSON object per line
    /// (reconnects on stream failure; Ctrl+C to stop).
    #[command(
        name = "listen_channel",
        aliases = ["listen-channel", "listen"]
    )]
    ListenChannel {
        /// Name of channel to listen to.
        channel_name: String,
    },
}

fn message_json(m: &Message) -> Value {
    json!({
        "message_id": m.message_id,
        "author_id": m.author_id,
        "channel_name": m.channel_name,
        "data": m.data,
    })
}

pub async fn run(uri: &str, app_key: &str, cmd: DeviceAgentCmd) -> CliResult {
    let client = DeviceAgentClient::connect(normalize_uri(uri))
        .await?
        .with_app_id(app_key);

    match cmd {
        DeviceAgentCmd::TestComms { message } => {
            let resp = client.test_comms(message).await?;
            print_json(&json!(resp));
        }
        DeviceAgentCmd::GetIsDdaAvailable => {
            // The status flags are derived from response headers, so probe
            // first (a fresh pydoover interface would always print False here).
            let _ = client.test_comms(DEFAULT_COMMS_MESSAGE).await;
            print_json(&json!(client.status().is_available()));
        }
        DeviceAgentCmd::GetIsDdaOnline => {
            let _ = client.test_comms(DEFAULT_COMMS_MESSAGE).await;
            print_json(&json!(client.status().is_online()));
        }
        DeviceAgentCmd::GetHasDdaBeenOnline => {
            let _ = client.test_comms(DEFAULT_COMMS_MESSAGE).await;
            print_json(&json!(client.status().has_been_online()));
        }
        DeviceAgentCmd::FetchChannelAggregate { channel_name } => {
            match client.fetch_channel_aggregate(&channel_name).await? {
                Some(data) => print_json(&data),
                None => return Err(format!("channel '{channel_name}' not found").into()),
            }
        }
        DeviceAgentCmd::UpdateChannelAggregate {
            channel_name,
            data,
            replace_data,
            clear_attachments,
            save_log,
            max_age_secs,
        } => {
            let opts = AggregateOptions {
                max_age_secs,
                save_log,
                replace_data,
                clear_attachments,
                ..Default::default()
            };
            client.update_channel_aggregate(&channel_name, &data, &opts).await?;
        }
        DeviceAgentCmd::CreateMessage { channel_name, data, timestamp } => {
            let id = match timestamp {
                Some(ts) => client.create_message_at(&channel_name, &data, ts).await?,
                None => client.create_message(&channel_name, &data).await?,
            };
            print_json(&json!(id));
        }
        DeviceAgentCmd::SendOneshotMessage { channel_name, data } => {
            client.send_one_shot_message(&channel_name, &data).await?;
            print_json(&json!(true));
        }
        DeviceAgentCmd::FetchMessage { channel_name, message_id } => {
            let message = client.fetch_message(&channel_name, message_id).await?;
            print_json(&message_json(&message));
        }
        DeviceAgentCmd::ListMessages { channel_name, before, after, limit, field_names } => {
            let opts = ListMessagesOptions { before, after, limit, field_names };
            let messages = client.list_messages(&channel_name, &opts).await?;
            print_json(&Value::Array(messages.iter().map(message_json).collect()));
        }
        DeviceAgentCmd::UpdateMessage {
            channel_name,
            message_id,
            data,
            replace_data,
            clear_attachments,
        } => {
            let opts = UpdateMessageOptions { replace_data, clear_attachments };
            let message = client.update_message(&channel_name, message_id, &data, &opts).await?;
            print_json(&message_json(&message));
        }
        DeviceAgentCmd::FetchTurnToken { camera_name } => {
            let c = client.fetch_turn_token(&camera_name).await?;
            print_json(&json!({
                "username": c.username,
                "credential": c.credential,
                "ttl": c.ttl,
                "expires_at": c.expires_at,
                "uris": c.uris,
            }));
        }
        DeviceAgentCmd::ListenChannel { channel_name } =>

            // Reconnect forever, as pydoover's stream_channel_events does.
            loop {
                match client.subscribe_events(&channel_name).await {
                    Ok(mut stream) => {
                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(event) => print_json(&json!({
                                    "event_name": event.event_name,
                                    "channel": event.channel,
                                    "payload": event.payload,
                                })),
                                Err(e) => {
                                    eprintln!("stream error on '{channel_name}': {e}");
                                    break;
                                }
                            }
                        }
                        eprintln!("event stream for '{channel_name}' ended; reconnecting");
                    }
                    Err(e) => eprintln!("failed to subscribe to '{channel_name}': {e}"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            },
    }
    Ok(())
}
