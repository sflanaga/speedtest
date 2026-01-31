use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ping,
    Download,
    Upload,
}

#[derive(Debug, Deserialize)]
pub struct ControlMsg {
    pub phase: Phase,
    #[serde(default)]
    pub action: Option<String>, // "start" for phases
    #[serde(default)]
    pub test_id: Option<u64>,
    #[serde(default)]
    pub seq: Option<u64>,
    #[serde(default)]
    pub t_send: Option<u64>,
    #[serde(default)]
    pub done: Option<bool>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub max_count: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<u64>,
    #[serde(default)]
    pub chunk_bytes: Option<usize>,
    #[serde(default)]
    pub bytes_sent: Option<u64>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub ping_count: Option<u64>,
    #[serde(default)]
    pub ping_avg_ms: Option<f64>,
    #[serde(default)]
    pub ping_min_ms: Option<f64>,
    #[serde(default)]
    pub ping_max_ms: Option<f64>,
}

#[derive(Debug)]
pub struct ValidationError {
    pub field: String,
    pub value: String,
    pub max: String,
    pub message: String,
}

impl ValidationError {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.message,
            "field": self.field,
            "value": self.value,
            "max": self.max
        })
    }
}

/// Tracks results for a single test run
#[derive(Debug, Default)]
pub struct TestResults {
    pub test_id: u64,
    pub ping_count: u64,
    pub ping_avg_ms: f64,
    pub ping_min_ms: f64,
    pub ping_max_ms: f64,
    pub download_bytes: u64,
    pub download_ms: u64,
    pub download_mbps: f64,
    pub upload_bytes: u64,
    pub upload_ms: u64,
    pub upload_mbps: f64,
    pub logged_complete: bool,
}
