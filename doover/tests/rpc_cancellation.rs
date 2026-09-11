//! RPC cancellation and in-flight progress (pydoover commits a21ee20 and
//! 4aeab8c).
//!
//! A cancellation has no status code of its own — the site patches the
//! command message to a terminal `error` carrying a marker — so most of what
//! is tested here is recognising that marker exactly as the site's own reader
//! does, and staying quiet once it arrives.
#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::Duration;

use doover::rpc::{command_is_cancelled, status_is_cancelled, RpcManager};
use doover::testing::MockBackend;
use doover::{ChannelBackend, Event};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

fn cancel_status() -> Value {
    json!({
        "code": "error",
        "message": {
            "info": "Command cancelled",
            "cancelled_at": 1787812081613i64,
            "cancelled_by": {"id": 7, "name": "Operator"},
        },
    })
}

#[test]
fn recognises_every_spelling_the_site_writes() {
    assert!(status_is_cancelled(&cancel_status()));
    // Older writers send a bare string, and the site tolerates the single-l
    // spelling in both forms.
    assert!(status_is_cancelled(
        &json!({"code": "error", "message": "Command cancelled"})
    ));
    assert!(status_is_cancelled(
        &json!({"code": "error", "message": " command canceled "})
    ));
    assert!(status_is_cancelled(
        &json!({"code": "error", "message": {"cancelled_at": 1}})
    ));

    // An ordinary failure is not a cancellation.
    assert!(!status_is_cancelled(
        &json!({"code": "error", "message": {"code": "BOOM", "message": "no"}})
    ));
    // Nor is a non-terminal status, whatever it says.
    assert!(!status_is_cancelled(
        &json!({"code": "pending", "message": {"cancelled_at": 1}})
    ));
    assert!(!status_is_cancelled(&json!({"code": "error"})));

    assert!(command_is_cancelled(&json!({"status": cancel_status()})));
    assert!(!command_is_cancelled(&json!({"status": {"code": "sent"}})));
}

fn request_event(message_id: u64, status: Value) -> Event {
    Event {
        event_name: "MessageCreate".to_string(),
        channel: "dv-rpc".to_string(),
        payload: json!({
            "id": message_id,
            "data": {
                "type": "rpc",
                "method": "slow",
                "request": {},
                "status": status,
                "response": {},
            },
        }),
    }
}

fn update_event(message_id: u64, status: Value) -> Event {
    Event {
        event_name: "MessageUpdate".to_string(),
        channel: "dv-rpc".to_string(),
        payload: json!({
            "id": message_id,
            "data": {"type": "rpc", "method": "slow", "status": status},
        }),
    }
}

/// A command already withdrawn before we got to it is dropped without
/// running the handler — a backlog delivered after a reconnect can carry both
/// the command and the cancellation, with the create arriving second.
#[tokio::test]
async fn a_command_cancelled_on_arrival_never_runs() {
    let backend = Arc::new(MockBackend::new());
    let rpc = RpcManager::new(backend.clone() as Arc<dyn ChannelBackend>, None);

    let (ran_tx, mut ran_rx) = tokio::sync::mpsc::unbounded_channel();
    rpc.register(Some("dv-rpc"), "slow", move |_ctx, _payload: Value| {
        let ran_tx = ran_tx.clone();
        async move {
            let _ = ran_tx.send(());
            Ok(json!({}))
        }
    });

    rpc.handle_event(&request_event(1, cancel_status())).await;

    assert!(ran_rx.try_recv().is_err(), "handler must not have run");
    assert!(
        backend.message_updates.lock().unwrap().is_empty(),
        "a cancelled command gets no response written over it"
    );
}

/// A cancellation arriving mid-flight reaches the running handler, and the
/// handler's own outcome is then not written over the canceller's record.
#[tokio::test]
async fn a_cancellation_mid_flight_reaches_the_handler_and_silences_the_response() {
    let backend = Arc::new(MockBackend::new());
    let rpc = Arc::new(RpcManager::new(
        backend.clone() as Arc<dyn ChannelBackend>,
        None,
    ));

    let (started_tx, started_rx) = oneshot::channel();
    let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let (saw_cancel_tx, saw_cancel_rx) = oneshot::channel();
    let saw_cancel_tx = Arc::new(std::sync::Mutex::new(Some(saw_cancel_tx)));

    rpc.register(Some("dv-rpc"), "slow", move |ctx, _payload: Value| {
        let started_tx = started_tx.clone();
        let saw_cancel_tx = saw_cancel_tx.clone();
        async move {
            // Progress before the cancellation is written through.
            ctx.progress(Some("Cranking…"), json!({"phase": "starting"}))
                .await
                .unwrap();
            if let Some(tx) = started_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            ctx.wait_cancelled().await;
            if let Some(tx) = saw_cancel_tx.lock().unwrap().take() {
                let _ = tx.send((ctx.cancelled_at(), ctx.cancelled_by()));
            }
            // A write attempted after cancellation is dropped, not sent.
            ctx.progress(Some("too late"), Value::Null).await.unwrap();
            ctx.error_if_cancelled()?;
            unreachable!("error_if_cancelled must unwind a cancelled command");
        }
    });

    let handler = {
        let rpc = rpc.clone();
        tokio::spawn(async move { rpc.handle_event(&request_event(2, json!({"code": "sent"}))).await })
    };
    timeout(WAIT, started_rx).await.unwrap().unwrap();

    rpc.handle_event(&update_event(2, cancel_status())).await;

    let (cancelled_at, cancelled_by) = timeout(WAIT, saw_cancel_rx).await.unwrap().unwrap();
    assert_eq!(cancelled_at, Some(1787812081613));
    assert_eq!(cancelled_by, Some(json!({"id": 7, "name": "Operator"})));

    timeout(WAIT, handler).await.unwrap().unwrap();

    let updates = backend.message_updates.lock().unwrap();
    assert_eq!(
        updates.len(),
        1,
        "only the pre-cancellation progress may be written, got {updates:?}"
    );
    assert_eq!(
        serde_json::to_string(&updates[0].data).unwrap(),
        r#"{"status":{"code":"pending","message":{"phase":"starting","text":"Cranking…"}}}"#
    );
}

/// The outbound side: a cancellation resolves a waiting call with the
/// dedicated code rather than an opaque error.
#[tokio::test]
async fn a_cancelled_call_surfaces_as_cancelled_not_an_opaque_error() {
    let backend = Arc::new(MockBackend::new());
    let rpc = Arc::new(RpcManager::new(
        backend.clone() as Arc<dyn ChannelBackend>,
        None,
    ));

    let call = {
        let rpc = rpc.clone();
        tokio::spawn(async move {
            rpc.call("slow", None, "dv-rpc", None, Some(Duration::from_secs(30)))
                .await
        })
    };

    // Wait for the request message the call wrote, then cancel it. The mock
    // hands out ids from 1, so the first message is 1.
    timeout(WAIT, async {
        while backend.messages().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("call never wrote its request message");
    let message_id = 1;

    rpc.handle_event(&update_event(message_id, cancel_status()))
        .await;

    let err = timeout(WAIT, call)
        .await
        .unwrap()
        .unwrap()
        .expect_err("a cancelled call must fail");
    assert!(
        err.to_string().contains("CANCELLED"),
        "expected a CANCELLED error, got {err}"
    );
}
