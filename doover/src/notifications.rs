//! Declarative notification schemas — the port of pydoover's
//! `pydoover/notifications/` (commit 9f5ad4e).
//!
//! An application declares the notifications it can send, the same way it
//! declares its tags, config and UI. Two things fall out of that declaration:
//!
//! * a canonical topic per notification, so the notification lands in the
//!   structured hierarchy rather than the temporary `legacy/default` bucket;
//! * a schema, exported at publish time and loaded onto the device at deploy,
//!   which is what lets the Doover site offer a per-notification opt-out
//!   instead of an all-or-nothing switch.
//!
//! ```ignore
//! #[derive(Notifications)]
//! struct MyNotifications {
//!     /// The battery has dropped below the configured cutoff.
//!     #[notification(message = "The battery is low", severity = "warn")]
//!     low_battery: NotificationDecl,
//!     #[notification(message = "Maintenance is due", policy = "opt_in")]
//!     maintenance_due: NotificationDecl,
//! }
//! ```
//!
//! Then, from the application:
//!
//! ```ignore
//! ctx.notify(MyNotifications::LOW_BATTERY).await?;
//! ctx.notify(MyNotifications::LOW_BATTERY.message("Battery at 10.2V")).await?;
//! ```

use serde_json::{Map, Value};

use crate::error::Result;
use crate::models::{NotificationPolicy, NotificationSeverity, NotificationTopic};

/// One declared notification an application can send (pydoover
/// `notifications.Notification`).
///
/// Note that severity is matched *before* topic: a subscriber whose
/// subscription severity is above this never receives the notification,
/// however they have set their topics.
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationDecl {
    /// Event name, forming the last topic segment. Defaults to the field name,
    /// matching the tags convention.
    pub event: &'static str,
    /// The default body, sent when the caller gives no message of its own.
    pub message: &'static str,
    /// Human label for the Doover site's notification picker. `None` falls
    /// back to a humanised form of the event name.
    pub display_name: Option<&'static str>,
    /// Longer explanation of when this notification fires, shown alongside the
    /// label.
    pub description: Option<&'static str>,
    pub severity: NotificationSeverity,
    /// Title / headline. Defaults server-side to the agent's display name.
    pub title: Option<&'static str>,
    /// Whether broad default subscriptions include this notification, or a
    /// subscriber has to ask for it. It is part of the topic, so changing it on
    /// a published app changes the topic and orphans any exclusion a subscriber
    /// had written against the old one.
    pub policy: NotificationPolicy,
}

impl NotificationDecl {
    pub const fn new(event: &'static str, message: &'static str) -> Self {
        Self {
            event,
            message,
            display_name: None,
            description: None,
            severity: NotificationSeverity::Info,
            title: None,
            policy: NotificationPolicy::Default,
        }
    }

    /// The schema entry for this notification (pydoover
    /// `Notification.to_schema`).
    ///
    /// The topic is deliberately absent: it needs the app *install* key, which
    /// differs per device, so it is assembled by whoever reads the schema from
    /// the install it is filed under.
    pub fn to_schema(&self) -> Value {
        let mut m = Map::new();
        m.insert("event".into(), Value::String(self.event.to_string()));
        m.insert(
            "display_name".into(),
            Value::String(
                self.display_name
                    .map(str::to_string)
                    .unwrap_or_else(|| humanise(self.event)),
            ),
        );
        m.insert("message".into(), Value::String(self.message.to_string()));
        m.insert(
            "severity".into(),
            Value::String(self.severity.wire().to_string()),
        );
        m.insert("policy".into(), Value::String(self.policy.wire().to_string()));
        if let Some(description) = self.description {
            m.insert("description".into(), Value::String(description.to_string()));
        }
        if let Some(title) = self.title {
            m.insert("title".into(), Value::String(title.to_string()));
        }
        Value::Object(m)
    }

    /// The canonical topic this notification is sent on, for `app_key`.
    pub fn topic(&self, app_key: &str) -> Result<NotificationTopic> {
        NotificationTopic::application(app_key, self.event, self.policy)
    }

    /// Build the notification to send, overriding the declared body.
    pub fn message(&self, message: impl Into<String>) -> PendingNotification {
        PendingNotification {
            decl: self.clone(),
            message: Some(message.into()),
            title: None,
            severity: None,
        }
    }
}

/// A declared notification about to be sent, with per-send overrides
/// (pydoover `BoundNotification.send`'s arguments).
#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub decl: NotificationDecl,
    pub message: Option<String>,
    pub title: Option<String>,
    pub severity: Option<NotificationSeverity>,
}

impl PendingNotification {
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn severity(mut self, severity: NotificationSeverity) -> Self {
        self.severity = Some(severity);
        self
    }

    /// The payload to send, with every field falling back to the declaration.
    pub fn build(&self, app_key: &str) -> Result<crate::models::Notification> {
        let mut n = crate::models::Notification::new(
            self.message
                .clone()
                .unwrap_or_else(|| self.decl.message.to_string()),
        )
        .severity(self.severity.unwrap_or(self.decl.severity))
        .topic(self.decl.topic(app_key)?.as_str());
        if let Some(title) = self.title.clone().or(self.decl.title.map(str::to_string)) {
            n = n.title(title);
        }
        Ok(n)
    }
}

impl From<NotificationDecl> for PendingNotification {
    fn from(decl: NotificationDecl) -> Self {
        Self {
            decl,
            message: None,
            title: None,
            severity: None,
        }
    }
}

/// Reflection over a set of declared notifications — implemented by
/// `#[derive(Notifications)]`, the counterpart of a pydoover
/// `notifications.Notifications` subclass.
pub trait NotificationSet {
    /// Every declared notification, in declaration (field) order.
    fn declarations() -> &'static [NotificationDecl];

    /// The schema for every declared notification, keyed by event name
    /// (pydoover `Notifications.to_schema`).
    fn to_schema() -> Value {
        let m: Map<String, Value> = Self::declarations()
            .iter()
            .map(|d| (d.event.to_string(), d.to_schema()))
            .collect();
        Value::Object(m)
    }

    /// The declaration with this event name, if any.
    fn get(event: &str) -> Option<&'static NotificationDecl> {
        Self::declarations().iter().find(|d| d.event == event)
    }
}

/// Apps with no declared notifications.
impl NotificationSet for () {
    fn declarations() -> &'static [NotificationDecl] {
        &[]
    }
}

fn humanise(name: &str) -> String {
    let spaced: String = name
        .chars()
        .map(|c| if c == '_' || c == '-' { ' ' } else { c })
        .collect();
    let spaced = spaced.trim();
    let mut chars = spaced.chars();
    match chars.next() {
        // pydoover uses str.capitalize(), which lowercases the rest.
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_entry_matches_pydoover_key_order() {
        let decl = NotificationDecl {
            description: Some("Fires below the cutoff."),
            title: Some("Battery"),
            severity: NotificationSeverity::Warn,
            policy: NotificationPolicy::OptIn,
            ..NotificationDecl::new("low_battery", "The battery is low")
        };
        assert_eq!(
            serde_json::to_string(&decl.to_schema()).unwrap(),
            serde_json::to_string(&json!({
                "event": "low_battery",
                "display_name": "Low battery",
                "message": "The battery is low",
                "severity": "Warn",
                "policy": "opt-in",
                "description": "Fires below the cutoff.",
                "title": "Battery",
            }))
            .unwrap()
        );
    }

    #[test]
    fn display_name_defaults_to_a_humanised_event_name() {
        let decl = NotificationDecl::new("maintenance_due", "Maintenance is due");
        assert_eq!(decl.to_schema()["display_name"], json!("Maintenance due"));
    }

    #[test]
    fn topic_is_canonical_and_carries_the_policy() {
        let decl = NotificationDecl {
            policy: NotificationPolicy::OptIn,
            ..NotificationDecl::new("low_battery", "x")
        };
        assert_eq!(
            decl.topic("pump_controller_1").unwrap().as_str(),
            "dev/applications/opt-in/pump_controller_1/low_battery"
        );
    }

    #[test]
    fn sending_falls_back_to_the_declaration() {
        let decl = NotificationDecl {
            title: Some("Battery"),
            severity: NotificationSeverity::Warn,
            ..NotificationDecl::new("low_battery", "The battery is low")
        };
        let n = PendingNotification::from(decl.clone()).build("app_1").unwrap();
        assert_eq!(n.message, "The battery is low");
        assert_eq!(n.title.as_deref(), Some("Battery"));
        assert_eq!(n.severity, Some(NotificationSeverity::Warn));

        let n = decl.message("Battery at 10.2V").build("app_1").unwrap();
        assert_eq!(n.message, "Battery at 10.2V");
        assert_eq!(n.title.as_deref(), Some("Battery"));
    }
}
