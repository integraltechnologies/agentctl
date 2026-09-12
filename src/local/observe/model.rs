use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub at_ms: u64,
    pub sessions: Vec<Session>,
    pub agents: Vec<Agent>,
    pub tasks: Vec<Task>,
    pub events: Vec<Event>,
    pub usage: Vec<Usage>,
    pub truncated: bool,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub ownership_known: bool,
    pub repository_id: String,
    pub workspace_id: String,
    pub root: String,
    pub supervisor_id: Option<String>,
    pub plans: Vec<String>,
    pub current_plan: Option<String>,
    pub title: String,
    pub state: String,
    pub verified: usize,
    pub task_count: usize,
    pub progress_complete: bool,
    pub correction_round: Option<u32>,
    pub blocker: Option<Blocker>,
    pub activity: String,
    pub check: Option<String>,
    pub last_event: Option<Event>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub session_id: Option<String>,
    pub parent_id: Option<String>,
    pub repository_id: String,
    pub workspace_id: String,
    pub root: String,
    pub role: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub plan_id: Option<String>,
    pub task_id: Option<String>,
    pub job_id: String,
    pub state: String,
    #[serde(default)]
    pub liveness: Liveness,
    pub activity: String,
    pub verification: Option<String>,
    pub blocker: Option<Blocker>,
    pub created_at_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub finished_at_ms: Option<u64>,
    pub last_event: Option<Event>,
    pub ownership_uncertain: bool,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Liveness {
    Live,
    #[default]
    Unknown,
}
impl Liveness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "LIVE",
            Self::Unknown => "UNKNOWN",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blocker {
    pub kind: String,
    pub description: String,
    pub dependencies: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub repository_id: String,
    pub workspace_id: String,
    pub session_id: Option<String>,
    pub plan_id: String,
    pub objective: String,
    pub dependencies: Vec<String>,
    pub lifecycle: String,
    pub presentation: String,
    pub blocker: Option<Blocker>,
    pub executor_job: Option<String>,
    pub verifier_job: Option<String>,
    pub attempts_in_view: usize,
    pub last_event: Option<Event>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub at_ms: u64,
    pub repository_id: String,
    pub workspace_id: Option<String>,
    pub plan_id: Option<String>,
    pub job_id: Option<String>,
    pub task_id: Option<String>,
    pub phase: String,
    pub summary: String,
    pub check: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub observation_id: String,
    pub at_ms: u64,
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub job_id: Option<String>,
    pub task_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub role: Option<String>,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub total: Option<u64>,
    pub provenance: crate::protocol::TokenUsageProvenance,
}

pub fn blocker(kind: &str, description: &str) -> Blocker {
    Blocker {
        kind: kind.into(),
        description: description.into(),
        dependencies: vec![],
    }
}

/// Compact terminal-safe labels, not raw logs, error payloads or transcripts.
pub fn label(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            if word.contains("sk-")
                || word.contains("eyJ")
                || word.contains("token=")
                || word.contains("key=")
            {
                "[REDACTED]".to_owned()
            } else {
                word.chars().filter(|c| !c.is_control()).collect::<String>()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect()
}
