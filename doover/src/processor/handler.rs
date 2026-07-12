//! Lambda entry points — pydoover `pydoover/processor/handler.py`.
//!
//! [`run_processor`] wires a [`Processor`] into `lambda_runtime` (deploy
//! with `cargo lambda build --release --arm64`; runtime `provided.al2023`).
//! Event unwrapping matches `handler.run_app`: an `aws:sns` record carries
//! the invocation JSON in `Records[0].Sns.Message` (with the subscription
//! ARN as the subscription ID); anything without `Records[0].EventSource`
//! passes through as-is (EventBridge schedules, direct invokes); any other
//! `EventSource` is an error.
//!
//! Python needs a module-level event-loop hack to survive Lambda recycling
//! its environment; the Rust runtime owns its own tokio reactor, so none of
//! that is needed. The `reqwest::Client` (connection pool) is shared across
//! warm invocations; processor state is fresh per invocation
//! (`P::default()`).

use std::sync::{Arc, OnceLock};

use lambda_runtime::LambdaEvent;
use serde_json::Value;

use crate::api::data::{DataClient, DATA_ENDPOINT_ENV, DEFAULT_DATA_ENDPOINT};
use crate::error::{DooverError, Result};

use super::application::Processor;
use super::runner::{run_invocation, LambdaMeta};

/// Options for driving one invocation outside Lambda (tests, local dev).
#[derive(Debug, Clone, Default)]
pub struct ProcessorOptions {
    /// Data API base URL; defaults to `$DOOVER_DATA_ENDPOINT` or
    /// `https://data.doover.com/api`.
    pub base_url: Option<String>,
    /// Reuse an existing HTTP connection pool.
    pub http_client: Option<reqwest::Client>,
    /// Invocation metadata for the summary (`requestId`, …).
    pub lambda: LambdaMeta,
}

/// The `reqwest::Client` shared across warm Lambda invocations.
fn shared_http() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new).clone()
}

/// Run a processor on AWS Lambda. Call from `main`:
///
/// ```no_run
/// # use doover::processor::{run_processor, Processor};
/// # #[derive(Default)] struct MyProcessor;
/// # impl Processor for MyProcessor {}
/// #[tokio::main]
/// async fn main() -> Result<(), lambda_runtime::Error> {
///     run_processor::<MyProcessor>().await
/// }
/// ```
pub async fn run_processor<P: Processor + Default>() -> std::result::Result<(), lambda_runtime::Error>
{
    lambda_runtime::run(lambda_runtime::service_fn(|event: LambdaEvent<Value>| async move {
        let (payload, context) = event.into_parts();
        let lambda = LambdaMeta {
            request_id: non_empty(context.request_id.clone()),
            function_name: non_empty(context.env_config.function_name.clone()),
            function_version: non_empty(context.env_config.version.clone()),
        };
        let options =
            ProcessorOptions { base_url: None, http_client: Some(shared_http()), lambda };
        handle_event_with::<P>(payload, options)
            .await
            .map_err(|e| -> lambda_runtime::Error { Box::new(e) })
    }))
    .await
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// Run one invocation locally (no Lambda): unwrap the event, drive the full
/// pipeline against the data API, and return the invocation-summary body.
pub async fn handle_event_local<P: Processor + Default>(event: Value) -> Result<Value> {
    handle_event_with::<P>(event, ProcessorOptions::default()).await
}

/// [`handle_event_local`] with explicit options (base URL / shared HTTP
/// client / lambda metadata) — what the tests and the Lambda wrapper use.
pub async fn handle_event_with<P: Processor + Default>(
    event: Value,
    options: ProcessorOptions,
) -> Result<Value> {
    let (data, subscription_id) = unwrap_event(event)?;

    let base_url = options
        .base_url
        .or_else(|| std::env::var(DATA_ENDPOINT_ENV).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| DEFAULT_DATA_ENDPOINT.to_string());
    let api = Arc::new(match options.http_client {
        Some(client) => DataClient::with_client(client, base_url),
        None => DataClient::with_base_url(base_url),
    });

    let mut processor = P::default();
    Ok(run_invocation(&mut processor, &data, subscription_id, api, &options.lambda).await)
}

/// pydoover `handler.run_app` event routing: SNS unwrap vs raw pass-through.
fn unwrap_event(event: Value) -> Result<(Value, Option<String>)> {
    let source = event
        .get("Records")
        .and_then(|r| r.get(0))
        .and_then(|r| r.get("EventSource"))
        .and_then(Value::as_str)
        .map(str::to_string);
    match source.as_deref() {
        Some("aws:sns") => {
            let record = &event["Records"][0];
            let message = record
                .get("Sns")
                .and_then(|s| s.get("Message"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    DooverError::InvalidPayload("SNS record missing Sns.Message".into())
                })?;
            let data: Value = serde_json::from_str(message)?;
            let subscription_id = record
                .get("EventSubscriptionArn")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok((data, subscription_id))
        }
        Some(_) => Err(DooverError::Other(
            "Unknown event. Must originate from SNS or EventBridge Schedules".into(),
        )),
        // No Records[0].EventSource: raw pass-through (schedules, direct).
        None => Ok((event, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sns_records_unwrap() {
        let inner = json!({"op": "on_schedule", "token": "t", "d": {}});
        let event = json!({
            "Records": [{
                "EventSource": "aws:sns",
                "EventSubscriptionArn": "arn:aws:sns:ap-southeast-2:1:topic:uuid",
                "Sns": {"Message": inner.to_string()},
            }]
        });
        let (data, sub) = unwrap_event(event).unwrap();
        assert_eq!(data, inner);
        assert_eq!(sub.as_deref(), Some("arn:aws:sns:ap-southeast-2:1:topic:uuid"));
    }

    #[test]
    fn raw_events_pass_through() {
        let event = json!({"op": "on_schedule", "token": "t", "d": {"schedule_id": "s-1"}});
        let (data, sub) = unwrap_event(event.clone()).unwrap();
        assert_eq!(data, event);
        assert_eq!(sub, None);
    }

    #[test]
    fn unknown_record_source_errors() {
        let event = json!({"Records": [{"EventSource": "aws:s3"}]});
        assert!(unwrap_event(event).is_err());
    }
}
