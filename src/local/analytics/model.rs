use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Job-cohort window [from_ms,to_ms). Usage is observed through the snapshot time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Query {
    pub repository: String,
    pub workspace: Option<String>,
    pub session: Option<String>,
    pub role: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub task: Option<String>,
    pub job: Option<String>,
    pub lifecycle: Option<String>,
    pub from_ms: u64,
    pub to_ms: u64,
    pub limit: usize,
}
impl Query {
    pub fn workspace(repository: String, workspace: String, now: u64) -> Self {
        Self {
            repository,
            workspace: Some(workspace),
            session: None,
            role: None,
            provider: None,
            model: None,
            task: None,
            job: None,
            lifecycle: None,
            from_ms: now.saturating_sub(7 * 86400000),
            to_ms: now,
            limit: 10000,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    pub exact: Option<u64>,
    pub estimated: Option<u64>,
    pub unknown: usize,
    pub overflow: bool,
}
impl Amount {
    pub fn quality(&self) -> &'static str {
        if self.overflow || self.unknown > 0 {
            if self.exact.is_some() || self.estimated.is_some() {
                "PARTIAL"
            } else {
                "UNKNOWN"
            }
        } else if self.exact.is_some() && self.estimated.is_some() {
            "MIXED"
        } else if self.exact.is_some() {
            "EXACT"
        } else if self.estimated.is_some() {
            "ESTIMATED"
        } else {
            "NOT_APPLICABLE"
        }
    }
    pub fn add(&mut self, value: Option<u64>, provenance: &str) {
        let Some(value) = value.filter(|_| provenance == "EXACT" || provenance == "ESTIMATED")
        else {
            self.unknown += 1;
            return;
        };
        let bucket = if provenance == "EXACT" {
            &mut self.exact
        } else {
            &mut self.estimated
        };
        match bucket.unwrap_or(0).checked_add(value) {
            Some(v) => *bucket = Some(v),
            None => {
                self.overflow = true;
                self.unknown += 1;
            }
        }
    }
    pub fn merge(&mut self, other: &Self) {
        if let Some(v) = other.exact {
            self.add(Some(v), "EXACT");
        }
        if let Some(v) = other.estimated {
            self.add(Some(v), "ESTIMATED");
        }
        self.unknown += other.unknown;
        self.overflow |= other.overflow;
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: Amount,
    pub output: Amount,
    pub cached_read: Amount,
    pub cache_write: Amount,
    pub reasoning: Amount,
    pub total: Amount,
}
impl Tokens {
    pub fn merge(&mut self, other: &Self) {
        self.input.merge(&other.input);
        self.output.merge(&other.output);
        self.cached_read.merge(&other.cached_read);
        self.cache_write.merge(&other.cache_write);
        self.reasoning.merge(&other.reasoning);
        self.total.merge(&other.total);
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Rate {
    pub numerator: usize,
    pub denominator: usize,
    pub ratio: Option<f64>,
}
impl Rate {
    pub fn new(numerator: usize, denominator: usize) -> Self {
        Self {
            numerator,
            denominator,
            ratio: (denominator > 0).then(|| numerator as f64 / denominator as f64),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Distribution {
    pub count: usize,
    pub unknown: usize,
    pub min: Option<u64>,
    pub median: Option<f64>,
    pub max: Option<u64>,
}
impl Distribution {
    pub fn of(values: impl Iterator<Item = Option<u64>>) -> Self {
        let mut known = vec![];
        let mut unknown = 0;
        for v in values {
            if let Some(v) = v {
                known.push(v)
            } else {
                unknown += 1;
            }
        }
        known.sort_unstable();
        let n = known.len();
        Self {
            count: n,
            unknown,
            min: known.first().copied(),
            max: known.last().copied(),
            median: if n == 0 {
                None
            } else {
                Some((known[(n - 1) / 2] as f64 + known[n / 2] as f64) / 2.0)
            },
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub agent: Option<String>,
    pub session: Option<String>,
    pub workspace: String,
    pub plan: Option<String>,
    pub task: Option<String>,
    pub role: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub lifecycle: String,
    pub correction_round: Option<u32>,
    pub parent: Option<String>,
    pub created_ms: Option<u64>,
    pub started_ms: Option<u64>,
    pub finished_ms: Option<u64>,
    pub wall_elapsed_ms: Option<u64>,
    pub completed_execution_ms: Option<u64>,
    pub active_execution_ms: Option<u64>,
    pub reported_decision: Option<String>,
    pub canonical_decision: Option<String>,
    pub accepted_executor: Option<String>,
    pub executor_disposition: Option<String>,
    pub tokens: Tokens,
    pub usage_quality: String,
    pub usage_observations: usize,
    pub prompt_bytes: Option<u64>,
    pub context_bytes: Option<u64>,
    pub instruction_bytes: Option<u64>,
    pub context_budget: Option<u64>,
    pub context_truncated: Option<bool>,
    pub route_attempt: Option<u64>,
    pub policy_skipped: Option<usize>,
    pub route_provenance: Option<RouteProvenance>,
    pub availability_failure: Option<String>,
    pub cost: Option<u64>,
}
/// Allowlisted historical route facts, never a copy of arbitrary metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteProvenance {
    pub provider_source: Option<String>,
    pub model_source: Option<String>,
    pub configured_provider_source: Option<String>,
    pub configured_model_source: Option<String>,
    pub fallback_source: Option<String>,
    pub explicit_override_fields: Vec<String>,
    pub actual_provider: Option<String>,
    pub actual_model: Option<String>,
    pub fallback_used: Option<bool>,
    pub fallback_depth: Option<u64>,
    pub preceding_failures: Vec<PrecedingFailure>,
    pub policy_skipped: Vec<RouteIdentifiers>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteIdentifiers {
    pub provider: Option<String>,
    pub model: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrecedingFailure {
    pub route: RouteIdentifiers,
    pub reason: Option<crate::local::runtime::routing::FailureClass>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Per-selected-job snapshot occurrences, not unique failed jobs. Kept
    /// separate from availability_failures to avoid double-counting attempts.
    pub preceding_failure_occurrences: BTreeMap<String, usize>,
    pub primary_executions: usize,
    pub unclassified_failed_jobs: usize,
    pub accepted_executor_attempts: usize,
    pub rejected_executor_attempts: usize,
    pub packet_accepted_usage: Tokens,
    pub packet_rejected_usage: Tokens,
    pub jobs: usize,
    pub contract_completed: usize,
    pub started_jobs: usize,
    pub exact_jobs: usize,
    pub estimated_jobs: usize,
    pub mixed_jobs: usize,
    pub partial_jobs: usize,
    pub unknown_jobs: usize,
    pub tokens: Tokens,
    pub token_quality: String,
    pub pass: usize,
    pub reject: usize,
    pub decisions_unknown: usize,
    pub reject_rate: Rate,
    pub associated_verified_tasks: usize,
    pub correction_jobs: usize,
    pub correction_rounds: BTreeMap<u32, usize>,
    pub correction_usage: BTreeMap<u32, Tokens>,
    pub route_resolutions: usize,
    pub fallback_attempts: usize,
    pub fallback_executions: usize,
    pub fallback_rate: Rate,
    pub fallback_depth: BTreeMap<u64, usize>,
    pub policy_skipped: usize,
    pub route_unknown: usize,
    pub availability_failures: BTreeMap<String, usize>,
    pub prompt_bytes: Distribution,
    pub context_bytes: Distribution,
    pub truncation_rate: Rate,
    pub truncation_unknown: usize,
    pub completed_execution_ms: Distribution,
    pub wall_elapsed_ms: Distribution,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub plan: String,
    pub workspace: String,
    pub session: Option<String>,
    pub lifecycle: String,
    pub dependencies: Vec<String>,
    pub correction_round: Option<u32>,
    pub correction_count: Option<usize>,
    pub executor_jobs: Vec<String>,
    pub verifier_jobs: Vec<String>,
    pub helper_jobs: Vec<String>,
    pub executor_usage: Tokens,
    pub verifier_usage: Tokens,
    pub helper_usage: Tokens,
    pub rejected_attempt_usage: Tokens,
    pub accepted_attempt_usage: Tokens,
    pub correction_round_usage: Tokens,
    pub summary: Summary,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub runtime_lifecycle: Option<String>,
    pub id: String,
    pub workspace: String,
    pub session: Option<String>,
    pub lifecycle: String,
    pub correction_round: Option<u32>,
    pub previous_plan: Option<String>,
    pub replaced_tasks: Vec<String>,
    pub task_ids: Vec<String>,
    pub verified_tasks: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace: String,
    pub plans: Vec<String>,
    pub lifecycle: String,
    pub current_tasks: usize,
    pub verified: usize,
    pub correction_round: Option<u32>,
    pub summary: Summary,
    pub integration: Summary,
    pub unattributed: Summary,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub role: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub summary: Summary,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub task_correction_distribution: BTreeMap<usize, usize>,
    pub as_of_ms: u64,
    pub providers: Vec<Group>,
    pub models: Vec<Group>,
    pub role_providers: Vec<Group>,
    pub analytics_version: u32,
    pub query: Query,
    pub truncated: bool,
    pub warnings: Vec<String>,
    pub summary: Summary,
    pub jobs: Vec<Job>,
    pub tasks: Vec<Task>,
    pub plans: Vec<Plan>,
    pub sessions: Vec<Session>,
    pub roles: Vec<Group>,
    pub routes: Vec<Group>,
    pub integration: Summary,
    pub unattributed: Summary,
    pub orphan_usage: Tokens,
    pub orphan_observations: usize,
    pub verified_task_usage: Tokens,
    pub verified_tasks_with_complete_usage: usize,
    pub verified_tasks_with_partial_usage: usize,
    pub tokens_per_complete_verified_task: Option<f64>,
    pub task_correction_rate: Rate,
    pub task_correction_unknown: usize,
}
