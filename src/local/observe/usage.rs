use super::*;
use crate::protocol::TokenUsageProvenance;
use std::collections::BTreeSet;

pub const WINDOW_MS: u64 = 600_000;
pub const RATE_MS: u64 = 60_000;
pub const STEP_MS: u64 = 10_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    #[default]
    Aggregate,
    Provider(String),
    Task(String),
    Role(String),
}
impl Scope {
    pub fn matches(&self, u: &Usage) -> bool {
        match self {
            Self::Aggregate => true,
            Self::Provider(v) => u.provider.as_ref() == Some(v),
            Self::Task(v) => u.task_id.as_ref() == Some(v),
            Self::Role(v) => u.role.as_ref() == Some(v),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatePoint {
    pub at_ms: u64,
    pub tokens_per_minute: Option<u64>,
    pub quality: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Series {
    pub scope: Scope,
    pub points: Vec<RatePoint>,
    pub total_observed: Option<u64>,
    pub quality: String,
    pub incomplete: bool,
    pub unreported_jobs: usize,
}
fn sum(samples: &[&Usage]) -> (Option<u64>, String) {
    let mut total = None::<u64>;
    let mut exact = false;
    let mut estimated = false;
    let mut unknown = false;
    for u in samples {
        if u.provenance == TokenUsageProvenance::Unknown || u.total.is_none() {
            unknown = true;
            continue;
        }
        match total.unwrap_or(0).checked_add(u.total.unwrap_or(0)) {
            Some(n) => total = Some(n),
            None => return (None, "UNKNOWN_OVERFLOW".into()),
        }
        exact |= u.provenance == TokenUsageProvenance::Exact;
        estimated |= u.provenance == TokenUsageProvenance::Estimated;
    }
    let quality = match (exact, estimated, unknown) {
        (false, false, _) => "UNKNOWN",
        (_, _, true) => "PARTIAL",
        (true, true, false) => "MIXED",
        (false, true, false) => "ESTIMATED",
        _ => "EXACT",
    };
    (total, quality.into())
}
/// Stage 0 usage records are deltas, never cumulative counters. Receipt-time bursts
/// are not interpolated into unobserved provider activity. Re-reading is idempotent.
pub fn series(snapshot: &Snapshot, scope: Scope, session: Option<&str>) -> Series {
    let now = snapshot.at_ms;
    let mut ids = BTreeSet::new();
    let mut observations: Vec<_> = snapshot
        .usage
        .iter()
        .filter(|u| {
            u.at_ms <= now
                && now.saturating_sub(u.at_ms) < WINDOW_MS + RATE_MS
                && scope.matches(u)
                && session.is_none_or(|s| u.session_id.as_deref() == Some(s))
        })
        .collect();
    observations.sort_by_key(|u| (u.at_ms, &u.observation_id));
    observations.retain(|u| ids.insert(u.observation_id.clone()));
    let mut points = vec![];
    for i in (0..=WINDOW_MS / STEP_MS).rev() {
        let Some(at_ms) = now.checked_sub(i * STEP_MS) else {
            continue;
        };
        let bucket: Vec<_> = observations
            .iter()
            .copied()
            .filter(|u| u.at_ms <= at_ms && at_ms.saturating_sub(u.at_ms) < RATE_MS)
            .collect();
        let (tokens_per_minute, quality) = sum(&bucket);
        points.push(RatePoint {
            at_ms,
            tokens_per_minute,
            quality,
        });
    }
    let visible: Vec<_> = observations
        .iter()
        .copied()
        .filter(|u| now.saturating_sub(u.at_ms) < WINDOW_MS)
        .collect();
    let (total_observed, mut quality) = sum(&visible);
    let unreported_jobs = snapshot
        .agents
        .iter()
        .filter(|a| {
            session.is_none_or(|s| a.session_id.as_deref() == Some(s))
                && match &scope {
                    Scope::Aggregate => true,
                    Scope::Provider(v) => a.provider.as_ref() == Some(v),
                    Scope::Task(v) => a.task_id.as_ref() == Some(v),
                    Scope::Role(v) => a.role == *v,
                }
                && (a.state == "RUNNING"
                    || a.state == "QUEUED"
                    || a.finished_at_ms
                        .is_some_and(|t| t <= now && now.saturating_sub(t) < WINDOW_MS))
                && !visible.iter().any(|u| {
                    u.agent_id.as_deref() == Some(&a.id)
                        && u.total.is_some()
                        && u.provenance != TokenUsageProvenance::Unknown
                })
        })
        .count();
    if unreported_jobs > 0 && total_observed.is_some() {
        quality = "PARTIAL".into();
    }
    Series {
        scope,
        points,
        total_observed,
        quality,
        incomplete: snapshot.truncated,
        unreported_jobs,
    }
}
