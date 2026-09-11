//! Declarative notification schemas (pydoover `pydoover/notifications/`,
//! commit 9f5ad4e) — `#[derive(Notifications)]`, the schema it emits, and
//! where that lands in `doover_config.json`.
//!
//! The expected schema is a byte-for-byte copy of what pydoover's
//! `Notifications.to_schema()` produces for the equivalent declaration.
#![cfg(feature = "macros")]

use doover::models::{NotificationPolicy, NotificationSeverity};
use doover::notifications::{NotificationDecl, NotificationSet, PendingNotification};
use doover::Notifications;
use serde_json::json;

#[derive(Notifications)]
struct MyNotifications {
    /// Fires below the cutoff.
    #[notification(
        message = "The battery is low",
        title = "Battery",
        severity = "warn",
        policy = "opt_in"
    )]
    low_battery: NotificationDecl,
    #[notification(message = "Maintenance is due")]
    maintenance_due: NotificationDecl,
}

#[test]
fn schema_matches_pydoover() {
    assert_eq!(
        serde_json::to_string(&MyNotifications::to_schema()).unwrap(),
        serde_json::to_string(&json!({
            "low_battery": {
                "event": "low_battery",
                "display_name": "Low battery",
                "message": "The battery is low",
                "severity": "Warn",
                "policy": "opt-in",
                "description": "Fires below the cutoff.",
                "title": "Battery",
            },
            "maintenance_due": {
                "event": "maintenance_due",
                "display_name": "Maintenance due",
                "message": "Maintenance is due",
                "severity": "Info",
                "policy": "default",
            },
        }))
        .unwrap()
    );
}

#[test]
fn declarations_are_exposed_as_constants() {
    assert_eq!(MyNotifications::LOW_BATTERY.event, "low_battery");
    assert_eq!(MyNotifications::LOW_BATTERY.policy, NotificationPolicy::OptIn);
    assert_eq!(
        MyNotifications::MAINTENANCE_DUE.severity,
        NotificationSeverity::Info
    );
    assert_eq!(MyNotifications::declarations().len(), 2);
    assert!(MyNotifications::get("low_battery").is_some());
    assert!(MyNotifications::get("nope").is_none());
}

#[test]
fn sending_uses_the_canonical_topic() {
    let payload = PendingNotification::from(MyNotifications::LOW_BATTERY)
        .build("pump_controller_1")
        .unwrap()
        .to_json();
    assert_eq!(
        payload,
        json!({
            "message": "The battery is low",
            "title": "Battery",
            "severity": NotificationSeverity::Warn as i64,
            "topic": "dev/applications/opt-in/pump_controller_1/low_battery",
        })
    );
}

#[test]
fn notification_schema_is_written_beside_the_others() {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "doover-rs-notification-export-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, r#"{"my_app": {"config_schema": {"a": 1}}}"#).unwrap();

    doover::config::write_notification_schema(&path, "my_app", MyNotifications::to_schema())
        .unwrap();

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let _ = std::fs::remove_file(&path);
    assert_eq!(written["my_app"]["config_schema"], json!({"a": 1}));
    assert_eq!(
        written["my_app"]["notification_schema"]["low_battery"]["policy"],
        json!("opt-in")
    );
}
