use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    /// Address a specific registered GUI for GUI-owned methods. Absent =
    /// primary GUI. Ignored for daemon-owned methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_client_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
    /// Provenance string — carried across the wire so the GUI's
    /// `TriggerEngine::try_promote_or_drop_preflight` can match on it.
    /// In particular, the action-registry-synthesized completion stamp
    /// (`nestty.action`) must survive the daemon→GUI round-trip so
    /// chained workflows advance after a daemon-hosted plugin replies.
    /// Absent on older wire clients; deserialized as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Daemon → GUI request. Discriminated from `Request` by the `invoke`
/// field (vs `method`). GUI replies with a normal `Response` echoing `id`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Invoke {
    pub id: String,
    pub invoke: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// Provenance of a bus event. Consumed by the trigger engine's security
/// gate (see `harness-integration.md` § Trust boundary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventOrigin {
    Internal,
    External,
}

impl Event {
    pub fn new(event_type: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            event_type: event_type.into(),
            data,
            source: None,
        }
    }

    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Stamp `data._origin` with the provenance. Plugin-supplied
    /// `_origin` keys are overwritten.
    pub fn with_origin(mut self, origin: EventOrigin) -> Self {
        let key = "_origin";
        if let Some(obj) = self.data.as_object_mut() {
            obj.insert(key.into(), serde_json::json!(origin));
        } else {
            self.data = serde_json::json!({ key: origin, "value": self.data });
        }
        self
    }
}

impl Request {
    pub fn new(
        id: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            method: method.into(),
            params,
            target_client_id: None,
        }
    }
}

impl Invoke {
    pub fn new(
        id: impl Into<String>,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            invoke: method.into(),
            params,
        }
    }
}

impl Response {
    pub fn success(id: String, result: serde_json::Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: String, code: &str, message: &str) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(ResponseError {
                code: code.to_string(),
                message: message.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_without_target_serializes_without_field() {
        let req = Request::new("x", "ping", json!({}));
        let s = serde_json::to_string(&req).unwrap();
        assert!(
            !s.contains("target_client_id"),
            "absent target must be omitted: {s}"
        );
    }

    #[test]
    fn request_with_target_serializes_field() {
        let mut req = Request::new("x", "tab.new", json!({}));
        req.target_client_id = Some("gui-1".into());
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"target_client_id\":\"gui-1\""));
    }

    #[test]
    fn old_request_format_still_parses() {
        // Wire from a pre-step-4 client (no target_client_id field).
        let s = r#"{"id":"x","method":"ping","params":{}}"#;
        let req: Request = serde_json::from_str(s).unwrap();
        assert!(req.target_client_id.is_none());
    }

    #[test]
    fn invoke_uses_invoke_field_not_method() {
        let inv = Invoke::new("d-1", "tab.list", json!({}));
        let s = serde_json::to_string(&inv).unwrap();
        assert!(s.contains("\"invoke\":\"tab.list\""));
        assert!(!s.contains("\"method\""));
    }

    #[test]
    fn event_with_origin_overwrites_existing_key() {
        let ev = Event::new("x", json!({ "_origin": "fake", "k": 1 }))
            .with_origin(EventOrigin::External);
        assert_eq!(ev.data["_origin"], json!("external"));
        assert_eq!(ev.data["k"], json!(1));
    }

    #[test]
    fn event_with_origin_wraps_non_object_data() {
        let ev = Event::new("x", json!(42)).with_origin(EventOrigin::Internal);
        assert_eq!(ev.data["_origin"], json!("internal"));
        assert_eq!(ev.data["value"], json!(42));
    }
}
