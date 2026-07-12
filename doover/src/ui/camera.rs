//! Camera elements — pydoover `ui/camera.py` ([`CameraLiveView`],
//! [`CameraHistory`]).

use serde_json::{Map, Value};

use super::element::{impl_element_common, ElementCommon};

/// Serialize the shared camera tail: `presets` (always, an array) and
/// `activePreset` (always, `null` unless set) — pydoover emits both
/// unconditionally.
fn camera_tail(m: &mut Map<String, Value>, presets: &[Value], active_preset: &Option<Value>) {
    m.insert("presets".into(), Value::Array(presets.to_vec()));
    m.insert("activePreset".into(), active_preset.clone().unwrap_or(Value::Null));
}

/// A live camera stream view (pydoover `ui.CameraLiveView`, type
/// `uiCameraLiveView`). pydoover defaults the display name to `"Live View"`
/// — use [`named`](Self::named) to override it.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraLiveView {
    pub common: ElementCommon,
    pub camera_name: String,
    pub stream_name: String,
    /// Emitted as `ptzControl`.
    pub allow_ptz_control: bool,
    /// pydoover initializes these empty/None; mutate for dynamic UIs.
    pub presets: Vec<Value>,
    pub active_preset: Option<Value>,
}

impl CameraLiveView {
    pub fn new(camera_name: &str, stream_name: &str, allow_ptz_control: bool) -> Self {
        Self::named("Live View", camera_name, stream_name, allow_ptz_control)
    }

    /// pydoover's `display_name=` keyword (the element name derives from it).
    pub fn named(
        display_name: &str,
        camera_name: &str,
        stream_name: &str,
        allow_ptz_control: bool,
    ) -> Self {
        Self {
            common: ElementCommon::new(display_name, Some(true)),
            camera_name: camera_name.to_string(),
            stream_name: stream_name.to_string(),
            allow_ptz_control,
            presets: Vec::new(),
            active_preset: None,
        }
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `CameraLiveView.to_dict()`: base keys + `cameraName`,
    /// `streamName`, `ptzControl`, `presets`, `activePreset`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiCameraLiveView");
        m.insert("cameraName".into(), Value::String(self.camera_name.clone()));
        m.insert("streamName".into(), Value::String(self.stream_name.clone()));
        m.insert("ptzControl".into(), Value::Bool(self.allow_ptz_control));
        camera_tail(&mut m, &self.presets, &self.active_preset);
        Value::Object(m)
    }
}

impl_element_common!(CameraLiveView);

/// A camera history browser (pydoover `ui.CameraHistory`, type
/// `uiCameraHistory`; `ptzControl` is hard-coded `true`). pydoover defaults
/// the display name to `"History"`.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraHistory {
    pub common: ElementCommon,
    pub camera_name: String,
    pub presets: Vec<Value>,
    pub active_preset: Option<Value>,
}

impl CameraHistory {
    pub fn new(camera_name: &str) -> Self {
        Self::named("History", camera_name)
    }

    /// pydoover's `display_name=` keyword.
    pub fn named(display_name: &str, camera_name: &str) -> Self {
        Self {
            common: ElementCommon::new(display_name, Some(true)),
            camera_name: camera_name.to_string(),
            presets: Vec::new(),
            active_preset: None,
        }
    }

    fn common(&self) -> &ElementCommon {
        &self.common
    }

    fn common_mut(&mut self) -> &mut ElementCommon {
        &mut self.common
    }

    /// pydoover `CameraHistory.to_dict()`: base keys + `cameraName`,
    /// `ptzControl` (always `true`), `presets`, `activePreset`.
    fn element_json(&self) -> Value {
        let mut m = self.common.base_json("uiCameraHistory");
        m.insert("cameraName".into(), Value::String(self.camera_name.clone()));
        m.insert("ptzControl".into(), Value::Bool(true));
        camera_tail(&mut m, &self.presets, &self.active_preset);
        Value::Object(m)
    }
}

impl_element_common!(CameraHistory);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiElement;
    use serde_json::json;

    #[test]
    fn live_view_defaults() {
        let c = CameraLiveView::new("front", "hls", true);
        let out = c.to_json();
        assert_eq!(UiElement::name(&c), "live_view");
        assert_eq!(out["displayString"], json!("Live View"));
        assert_eq!(out["cameraName"], json!("front"));
        assert_eq!(out["streamName"], json!("hls"));
        assert_eq!(out["ptzControl"], json!(true));
        assert_eq!(out["presets"], json!([]));
        assert_eq!(out["activePreset"], Value::Null);
    }

    #[test]
    fn history_forces_ptz_true() {
        let c = CameraHistory::new("front");
        let out = c.to_json();
        assert_eq!(UiElement::name(&c), "history");
        assert_eq!(out["ptzControl"], json!(true));
        assert!(out.get("streamName").is_none());
    }
}
