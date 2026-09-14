//! Stage 9B structured experiment facts. These records are deliberately factual:
//! this module has no threshold evaluation, planner hook, or process-control path.
use super::*;
use serde_json::Value as JsonValue;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::Read,
};

pub const EVENT_FILE_ENV: &str = "AGENTCTL_EVENT_FILE";
pub const MAX_EVENT_FRAME_BYTES: usize = 64 * 1024;
pub const DEFAULT_EVENT_QUERY_LIMIT: usize = 1_000;
pub const MAX_EVENT_QUERY_LIMIT: usize = 10_000;
const READ_BYTES_PER_POLL: usize = 256 * 1024;
const MAX_CHECKPOINT_HASH_BYTES: u64 = 64 * 1024 * 1024;

/// Path components that always name administrative/control-plane storage, independent of
/// user-configured protected paths: Git's own directory and agentctl's own control-plane
/// directory. Checkpoints must never be able to present these as ordinary workspace files.
const CONTROL_PLANE_COMPONENTS: [&str; 2] = [".git", ".agentctl"];

/// Canonicalized directories a checkpoint path must not resolve into, covering both Git's
/// administrative storage and agentctl's own control-plane directory (when present).
fn control_plane_directories(info: &RepositoryInfo) -> Vec<PathBuf> {
    let mut dirs = vec![info.git_directory.clone(), info.common_directory.clone()];
    if let Ok(canonical) = std::fs::canonicalize(info.root.join(".agentctl")) {
        dirs.push(canonical);
    }
    dirs
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum ExperimentEventData {
    Metric {
        name: String,
        value: f64,
        step: Option<u64>,
        epoch: Option<u64>,
        index: Option<u64>,
        unit: Option<String>,
        tags: BTreeMap<String, String>,
    },
    Checkpoint {
        name: String,
        path: String,
        step: Option<u64>,
        epoch: Option<u64>,
        byte_size: u64,
        modified_at_ms: Option<u64>,
        content_hash: Option<String>,
    },
    Health {
        kind: ExperimentHealthKind,
        message: Option<String>,
    },
    ProcessStatus {
        status: String,
        message: Option<String>,
    },
}

impl ExperimentEventData {
    fn event_type(&self) -> &'static str {
        match self {
            Self::Metric { .. } => "METRIC",
            Self::Checkpoint { .. } => "CHECKPOINT",
            Self::Health { .. } => "HEALTH",
            Self::ProcessStatus { .. } => "PROCESS_STATUS",
        }
    }
    fn metric_name(&self) -> Option<&str> {
        match self {
            Self::Metric { name, .. } => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExperimentHealthKind {
    Heartbeat,
    Warning,
    Error,
    IngestionError,
    NonfiniteMetric,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentRuntimeEvent {
    pub arrival_sequence: i64,
    pub experiment_id: ExperimentId,
    pub workspace_id: WorkspaceId,
    pub attempt: u32,
    pub channel: String,
    pub source_sequence: u64,
    pub timestamp_ms: u64,
    pub observed_at_ms: u64,
    pub source: String,
    pub event: ExperimentEventData,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperimentEventSummary {
    pub event_count: u64,
    pub last_event_timestamp_ms: Option<u64>,
    pub latest_metrics: BTreeMap<String, f64>,
    pub latest_checkpoint: Option<String>,
    pub ingestion_errors: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ExperimentEventQuery {
    pub attempt: Option<u32>,
    pub event_type: Option<String>,
    pub metric_name: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum EventFrame {
    Metric {
        sequence: u64,
        timestamp_ms: u64,
        source: String,
        name: String,
        value: f64,
        #[serde(default)]
        step: Option<u64>,
        #[serde(default)]
        epoch: Option<u64>,
        #[serde(default)]
        index: Option<u64>,
        #[serde(default)]
        unit: Option<String>,
        #[serde(default)]
        tags: BTreeMap<String, String>,
    },
    Checkpoint {
        sequence: u64,
        timestamp_ms: u64,
        source: String,
        name: String,
        path: String,
        #[serde(default)]
        step: Option<u64>,
        #[serde(default)]
        epoch: Option<u64>,
    },
    Health {
        sequence: u64,
        timestamp_ms: u64,
        source: String,
        kind: ExperimentHealthKind,
        #[serde(default)]
        message: Option<String>,
    },
    Status {
        sequence: u64,
        timestamp_ms: u64,
        source: String,
        status: String,
        #[serde(default)]
        message: Option<String>,
    },
}

#[derive(Clone)]
struct Draft {
    channel: &'static str,
    source_sequence: u64,
    timestamp_ms: u64,
    observed_at_ms: u64,
    source: String,
    event: ExperimentEventData,
    frame_hash: String,
    line_number: u64,
}

fn bounded(value: &str, name: &str, max: usize) -> Result<()> {
    require(!value.trim().is_empty(), format!("{name} cannot be blank"))?;
    require(value.len() <= max, format!("{name} exceeds {max} bytes"))?;
    require(
        !value.chars().any(char::is_control),
        format!("{name} contains control characters"),
    )
}

fn metric_name(name: &str) -> Result<()> {
    bounded(name, "metric name", 128)?;
    require(
        name.as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_'),
        "metric name must start with an ASCII letter, digit, or '_'",
    )?;
    require(
        name.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b)),
        "metric name may contain only ASCII letters, digits, '_', '.', '/', and '-'",
    )
}

fn validate_common(sequence: u64, timestamp_ms: u64, source: &str) -> Result<()> {
    require(
        sequence <= i64::MAX as u64,
        "event sequence exceeds SQLite range",
    )?;
    require(
        timestamp_ms <= i64::MAX as u64,
        "event timestamp exceeds SQLite range",
    )?;
    bounded(source, "event source", 128)
}

fn checkpoint(
    info: &RepositoryInfo,
    policy: &ProjectConfig,
    name: String,
    path: String,
    step: Option<u64>,
    epoch: Option<u64>,
) -> Result<ExperimentEventData> {
    bounded(&name, "checkpoint name", 128)?;
    require(path.len() <= 1_024, "checkpoint path exceeds 1024 bytes")?;
    crate::validation::repo_path(&path)?;
    require(
        !path
            .split('/')
            .any(|part| CONTROL_PLANE_COMPONENTS.contains(&part)),
        "checkpoint path may not enter Git or agentctl administrative storage",
    )?;
    for rule in &policy.protected {
        if path == rule.path || path.starts_with(&format!("{}/", rule.path)) {
            return Err(Error::Invalid(format!(
                "checkpoint path intersects protected path {}",
                rule.path
            )));
        }
    }
    let canonical = std::fs::canonicalize(info.root.join(&path))?;
    require(
        canonical.starts_with(&info.root),
        "checkpoint path escapes workspace",
    )?;
    require(
        !control_plane_directories(info)
            .iter()
            .any(|dir| canonical.starts_with(dir)),
        "checkpoint path resolves into Git or agentctl administrative storage",
    )?;
    for rule in &policy.protected {
        if let Ok(protected) = std::fs::canonicalize(info.root.join(&rule.path)) {
            require(
                canonical != protected && !canonical.starts_with(&protected),
                format!("checkpoint path resolves into protected path {}", rule.path),
            )?;
        }
    }
    let before = canonical.metadata()?;
    require(before.is_file(), "checkpoint path must name a regular file")?;
    let byte_size = before.len();
    let modified_at_ms = before
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| u64::try_from(d.as_millis()).ok());
    let content_hash = if byte_size <= MAX_CHECKPOINT_HASH_BYTES {
        let mut file = File::open(&canonical)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let after = canonical.metadata()?;
        if after.len() == before.len() && after.modified().ok() == before.modified().ok() {
            Some(format!("blake3:{}", hasher.finalize().to_hex()))
        } else {
            None
        }
    } else {
        None
    };
    Ok(ExperimentEventData::Checkpoint {
        name,
        path,
        step,
        epoch,
        byte_size,
        modified_at_ms,
        content_hash,
    })
}

fn validate_frame(
    frame: EventFrame,
    info: &RepositoryInfo,
    policy: &ProjectConfig,
) -> Result<(u64, u64, String, ExperimentEventData)> {
    let (sequence, timestamp_ms, source, event) = match frame {
        EventFrame::Metric {
            sequence,
            timestamp_ms,
            source,
            name,
            value,
            step,
            epoch,
            index,
            unit,
            tags,
        } => {
            metric_name(&name)?;
            require(value.is_finite(), "metric value must be finite")?;
            if let Some(unit) = &unit {
                bounded(unit, "metric unit", 32)?;
            }
            require(tags.len() <= 16, "metric tags exceed 16 entries")?;
            for (key, value) in &tags {
                bounded(key, "metric tag key", 64)?;
                bounded(value, "metric tag value", 256)?;
            }
            (
                sequence,
                timestamp_ms,
                source,
                ExperimentEventData::Metric {
                    name,
                    value,
                    step,
                    epoch,
                    index,
                    unit,
                    tags,
                },
            )
        }
        EventFrame::Checkpoint {
            sequence,
            timestamp_ms,
            source,
            name,
            path,
            step,
            epoch,
        } => (
            sequence,
            timestamp_ms,
            source,
            checkpoint(info, policy, name, path, step, epoch)?,
        ),
        EventFrame::Health {
            sequence,
            timestamp_ms,
            source,
            kind,
            message,
        } => {
            require(
                !matches!(
                    kind,
                    ExperimentHealthKind::IngestionError | ExperimentHealthKind::NonfiniteMetric
                ),
                "controller-reserved health kind",
            )?;
            if let Some(message) = &message {
                bounded(message, "health message", 512)?;
            }
            (
                sequence,
                timestamp_ms,
                source,
                ExperimentEventData::Health { kind, message },
            )
        }
        EventFrame::Status {
            sequence,
            timestamp_ms,
            source,
            status,
            message,
        } => {
            bounded(&status, "process status", 64)?;
            if let Some(message) = &message {
                bounded(message, "process status message", 512)?;
            }
            (
                sequence,
                timestamp_ms,
                source,
                ExperimentEventData::ProcessStatus { status, message },
            )
        }
    };
    validate_common(sequence, timestamp_ms, &source)?;
    Ok((sequence, timestamp_ms, source, event))
}

fn ingestion_error(line_number: u64, message: impl Into<String>, nonfinite: bool) -> Result<Draft> {
    let now = now_ms()?;
    let mut message = message.into();
    if message.len() > 512 {
        let mut end = 512;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    let event = ExperimentEventData::Health {
        kind: if nonfinite {
            ExperimentHealthKind::NonfiniteMetric
        } else {
            ExperimentHealthKind::IngestionError
        },
        message: Some(message),
    };
    Ok(Draft {
        channel: "INGESTION",
        source_sequence: line_number,
        timestamp_ms: now,
        observed_at_ms: now,
        source: "agentctl".into(),
        frame_hash: format!(
            "blake3:{}",
            blake3::hash(&line_number.to_le_bytes()).to_hex()
        ),
        event,
        line_number,
    })
}

fn decode_line(
    line: &[u8],
    line_number: u64,
    info: &RepositoryInfo,
    policy: &ProjectConfig,
) -> Result<Draft> {
    let observed_at_ms = now_ms()?;
    let frame_hash = format!("blake3:{}", blake3::hash(line).to_hex());
    let parsed: std::result::Result<JsonValue, _> = serde_json::from_slice(line);
    let nonfinite = parsed.as_ref().ok().is_some_and(|value| {
        value.get("type").and_then(JsonValue::as_str) == Some("metric")
            && value.get("value").is_some_and(|value| {
                value.is_number() && value.as_f64().is_none()
                    || value.as_str().is_some_and(|v| {
                        matches!(v, "NaN" | "+Inf" | "-Inf" | "Infinity" | "-Infinity")
                    })
            })
    });
    let frame: EventFrame = match parsed.and_then(serde_json::from_value) {
        Ok(frame) => frame,
        Err(error) => {
            return ingestion_error(
                line_number,
                format!("rejected event frame: {error}"),
                nonfinite,
            );
        }
    };
    match validate_frame(frame, info, policy) {
        Ok((source_sequence, timestamp_ms, source, event)) => Ok(Draft {
            channel: "EVENT_FILE",
            source_sequence,
            timestamp_ms,
            observed_at_ms,
            source,
            event,
            frame_hash,
            line_number,
        }),
        Err(error) => ingestion_error(
            line_number,
            format!("rejected event frame: {error}"),
            nonfinite,
        ),
    }
}

pub(super) struct EventIngestor {
    file: File,
    pending: Vec<u8>,
    discarding_oversize: bool,
    line_number: u64,
    queued: Vec<Draft>,
}

impl EventIngestor {
    pub(super) fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        Ok(Self {
            file,
            pending: Vec::with_capacity(4096),
            discarding_oversize: false,
            line_number: 0,
            queued: vec![],
        })
    }

    pub(super) fn drain(
        &mut self,
        store: &mut Store,
        info: &RepositoryInfo,
        policy: &ProjectConfig,
        id: &ExperimentId,
        attempt: u32,
        final_drain: bool,
    ) -> Result<(usize, bool)> {
        let mut inserted = 0;
        if !self.queued.is_empty() {
            inserted += persist(store, info, id, attempt, &self.queued)?;
            self.queued.clear();
        }
        let mut drafts = vec![];
        let mut read_total = 0;
        let mut reached_eof = false;
        // The small read buffer also bounds the number of newline-only malformed
        // frames materialized in one batch. Normal metric throughput remains far
        // above the intended few-events-per-second workload.
        let mut buffer = [0_u8; 512];
        while read_total < READ_BYTES_PER_POLL && drafts.len() < 512 {
            let n = self.file.read(&mut buffer)?;
            if n == 0 {
                reached_eof = true;
                break;
            }
            read_total += n;
            for byte in &buffer[..n] {
                if *byte == b'\n' {
                    self.line_number += 1;
                    if self.discarding_oversize {
                        self.discarding_oversize = false;
                    } else {
                        let line = std::mem::take(&mut self.pending);
                        if line.is_empty() {
                            drafts.push(ingestion_error(
                                self.line_number,
                                "rejected empty event frame",
                                false,
                            )?);
                        } else {
                            drafts.push(decode_line(&line, self.line_number, info, policy)?);
                        }
                    }
                } else if !self.discarding_oversize {
                    if self.pending.len() == MAX_EVENT_FRAME_BYTES {
                        self.pending.clear();
                        self.discarding_oversize = true;
                        drafts.push(ingestion_error(
                            self.line_number + 1,
                            format!(
                                "rejected event frame larger than {MAX_EVENT_FRAME_BYTES} bytes"
                            ),
                            false,
                        )?);
                    } else {
                        self.pending.push(*byte);
                    }
                }
            }
        }
        if final_drain && reached_eof && !self.pending.is_empty() {
            self.line_number += 1;
            self.pending.clear();
            drafts.push(ingestion_error(
                self.line_number,
                "rejected partial event frame at process exit",
                false,
            )?);
        }
        let caught_up = reached_eof;
        self.queued = drafts;
        inserted += persist(store, info, id, attempt, &self.queued)?;
        self.queued.clear();
        Ok((inserted, caught_up))
    }
}

fn persist(
    store: &mut Store,
    info: &RepositoryInfo,
    id: &ExperimentId,
    attempt: u32,
    drafts: &[Draft],
) -> Result<usize> {
    if drafts.is_empty() {
        return Ok(0);
    }
    let tx = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut inserted = 0;
    let mut conflicts = vec![];
    for draft in drafts {
        let event = ExperimentRuntimeEvent {
            arrival_sequence: 0,
            experiment_id: id.clone(),
            workspace_id: info.workspace_id.clone(),
            attempt,
            channel: draft.channel.into(),
            source_sequence: draft.source_sequence,
            timestamp_ms: draft.timestamp_ms,
            observed_at_ms: draft.observed_at_ms,
            source: draft.source.clone(),
            event: draft.event.clone(),
        };
        let changed = tx.execute(
            "INSERT OR IGNORE INTO experiment_events(repo_id,workspace_id,experiment_id,attempt,channel,source_sequence,event_type,metric_name,timestamp_ms,observed_at_ms,frame_hash,event_json) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str(), i64::from(attempt),
                draft.channel, i64::try_from(draft.source_sequence).map_err(|e| Error::Invalid(e.to_string()))?,
                event.event.event_type(), event.event.metric_name(),
                i64::try_from(draft.timestamp_ms).map_err(|e| Error::Invalid(e.to_string()))?,
                i64::try_from(draft.observed_at_ms).map_err(|e| Error::Invalid(e.to_string()))?,
                draft.frame_hash, serde_json::to_string(&event)?
            ],
        )?;
        if changed == 1 {
            inserted += 1;
        } else if draft.channel == "EVENT_FILE" {
            let existing_hash: String = tx.query_row(
                "SELECT frame_hash FROM experiment_events WHERE repo_id=?1 AND experiment_id=?2 AND attempt=?3 AND channel=?4 AND source_sequence=?5",
                params![info.repository_id.as_str(), id.as_str(), i64::from(attempt), draft.channel, i64::try_from(draft.source_sequence).map_err(|e| Error::Invalid(e.to_string()))?],
                |row| row.get(0),
            )?;
            if existing_hash != draft.frame_hash {
                conflicts.push(draft.line_number);
            }
        }
    }
    for line in conflicts {
        let draft = ingestion_error(
            line,
            "rejected conflicting replay for an existing source sequence",
            false,
        )?;
        let event = ExperimentRuntimeEvent {
            arrival_sequence: 0,
            experiment_id: id.clone(),
            workspace_id: info.workspace_id.clone(),
            attempt,
            channel: draft.channel.into(),
            source_sequence: draft.source_sequence,
            timestamp_ms: draft.timestamp_ms,
            observed_at_ms: draft.observed_at_ms,
            source: draft.source,
            event: draft.event,
        };
        inserted += tx.execute(
            "INSERT OR IGNORE INTO experiment_events(repo_id,workspace_id,experiment_id,attempt,channel,source_sequence,event_type,metric_name,timestamp_ms,observed_at_ms,frame_hash,event_json) VALUES (?1,?2,?3,?4,?5,?6,'HEALTH',NULL,?7,?8,?9,?10)",
            params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str(), i64::from(attempt), draft.channel, i64::try_from(draft.source_sequence).map_err(|e| Error::Invalid(e.to_string()))?, i64::try_from(draft.timestamp_ms).map_err(|e| Error::Invalid(e.to_string()))?, i64::try_from(draft.observed_at_ms).map_err(|e| Error::Invalid(e.to_string()))?, draft.frame_hash, serde_json::to_string(&event)?],
        )?;
    }
    tx.commit()?;
    Ok(inserted)
}

impl Store {
    pub fn experiment_events(
        &self,
        root: &Path,
        id: &ExperimentId,
        query: &ExperimentEventQuery,
    ) -> Result<Vec<ExperimentRuntimeEvent>> {
        let info = graph::checked_workspace(self, root)?;
        let limit = if query.limit == 0 {
            DEFAULT_EVENT_QUERY_LIMIT
        } else {
            query.limit
        };
        require(
            limit <= MAX_EVENT_QUERY_LIMIT,
            format!("event query limit must be at most {MAX_EVENT_QUERY_LIMIT}"),
        )?;
        require(query.attempt != Some(0), "attempt must be at least 1")?;
        if let Some(name) = &query.metric_name {
            metric_name(name)?;
        }
        if let Some(kind) = &query.event_type {
            require(
                matches!(
                    kind.as_str(),
                    "METRIC" | "CHECKPOINT" | "HEALTH" | "PROCESS_STATUS"
                ),
                "unknown experiment event type",
            )?;
        }
        let limit = i64::try_from(limit).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut sql = "SELECT arrival_sequence,event_json FROM experiment_events WHERE repo_id=? AND workspace_id=? AND experiment_id=?".to_string();
        let mut values = vec![
            rusqlite::types::Value::Text(info.repository_id.as_str().to_owned()),
            rusqlite::types::Value::Text(info.workspace_id.as_str().to_owned()),
            rusqlite::types::Value::Text(id.as_str().to_owned()),
        ];
        if let Some(attempt) = query.attempt {
            sql.push_str(" AND attempt=?");
            values.push(rusqlite::types::Value::Integer(i64::from(attempt)));
        }
        if let Some(kind) = &query.event_type {
            // `kind` was allowlisted above. Keeping the value literal lets SQLite
            // prove the METRIC partial-index predicate during statement planning.
            sql.push_str(match kind.as_str() {
                "METRIC" => " AND event_type='METRIC'",
                "CHECKPOINT" => " AND event_type='CHECKPOINT'",
                "HEALTH" => " AND event_type='HEALTH'",
                "PROCESS_STATUS" => " AND event_type='PROCESS_STATUS'",
                _ => unreachable!("validated event type"),
            });
        }
        if let Some(name) = &query.metric_name {
            if query.event_type.is_none() {
                sql.push_str(" AND event_type='METRIC'");
            }
            sql.push_str(" AND metric_name=?");
            values.push(rusqlite::types::Value::Text(name.clone()));
        }
        sql.push_str(" ORDER BY arrival_sequence DESC LIMIT ?");
        values.push(rusqlite::types::Value::Integer(limit));
        let mut rows: Vec<(i64, String)> = self
            .connection
            .prepare(&sql)?
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows.reverse();
        rows.into_iter()
            .map(|(sequence, json)| {
                let mut event: ExperimentRuntimeEvent = serde_json::from_str(&json)?;
                event.arrival_sequence = sequence;
                Ok(event)
            })
            .collect()
    }

    pub(super) fn experiment_event_summary(
        &self,
        info: &RepositoryInfo,
        id: &ExperimentId,
    ) -> Result<ExperimentEventSummary> {
        let (count, errors): (i64, i64) = self.connection.query_row(
            "SELECT count(*),coalesce(sum(CASE WHEN event_type='HEALTH' AND json_extract(event_json,'$.event.kind') IN ('INGESTION_ERROR','NONFINITE_METRIC') THEN 1 ELSE 0 END),0) FROM experiment_events WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3",
            params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let last: Option<i64> = self.connection.query_row(
            "SELECT timestamp_ms FROM experiment_events WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 ORDER BY arrival_sequence DESC LIMIT 1",
            params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str()],
            |row| row.get(0),
        ).optional()?;
        let mut latest_metrics = BTreeMap::new();
        let mut statement = self.connection.prepare(
            "SELECT metric_name,json_extract(event_json,'$.event.value') FROM experiment_events e WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 AND event_type='METRIC' AND arrival_sequence=(SELECT max(arrival_sequence) FROM experiment_events WHERE repo_id=e.repo_id AND experiment_id=e.experiment_id AND metric_name=e.metric_name)",
        )?;
        for row in statement.query_map(
            params![
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                id.as_str()
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)),
        )? {
            let (name, value) = row?;
            latest_metrics.insert(name, value);
        }
        let latest_checkpoint = self.connection.query_row(
            "SELECT json_extract(event_json,'$.event.path') FROM experiment_events WHERE repo_id=?1 AND workspace_id=?2 AND experiment_id=?3 AND event_type='CHECKPOINT' ORDER BY arrival_sequence DESC LIMIT 1",
            params![info.repository_id.as_str(), info.workspace_id.as_str(), id.as_str()],
            |row| row.get(0),
        ).optional()?;
        Ok(ExperimentEventSummary {
            event_count: u64::try_from(count).map_err(|e| Error::Invalid(e.to_string()))?,
            last_event_timestamp_ms: last
                .map(u64::try_from)
                .transpose()
                .map_err(|e| Error::Invalid(e.to_string()))?,
            latest_metrics,
            latest_checkpoint,
            ingestion_errors: u64::try_from(errors).map_err(|e| Error::Invalid(e.to_string()))?,
        })
    }
}
