//! Descriptive cohort analytics. No policy lookup, artifact reads or writes.
mod cli;
mod model;
mod query;
pub(crate) use cli::run;
pub use model::*;
use std::collections::{BTreeMap, BTreeSet};

pub fn summarize(jobs: &[&Job]) -> Summary {
    let mut s = Summary::default();
    let mut verified = BTreeSet::new();
    for j in jobs {
        if let Some(route) = &j.route_provenance {
            for failure in &route.preceding_failures {
                let reason = failure
                    .reason
                    .and_then(|r| serde_json::to_value(r).ok())
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "UNKNOWN".into());
                *s.preceding_failure_occurrences.entry(reason).or_default() += 1;
            }
        }
        s.primary_executions += usize::from(j.route_attempt == Some(0) && j.started_ms.is_some());
        s.unclassified_failed_jobs +=
            usize::from(j.lifecycle == "FAILED" && j.availability_failure.is_none());
        s.accepted_executor_attempts +=
            usize::from(j.executor_disposition.as_deref() == Some("PASS"));
        s.rejected_executor_attempts +=
            usize::from(j.executor_disposition.as_deref() == Some("REJECT"));
        if j.task.is_some() {
            match j
                .executor_disposition
                .as_deref()
                .or(j.canonical_decision.as_deref())
            {
                Some("PASS") => s.packet_accepted_usage.merge(&j.tokens),
                Some("REJECT") => s.packet_rejected_usage.merge(&j.tokens),
                _ => {}
            }
        }
        s.jobs += 1;
        s.contract_completed += usize::from(j.lifecycle == "SUCCEEDED");
        s.started_jobs += usize::from(j.started_ms.is_some());
        s.tokens.merge(&j.tokens);
        match j.usage_quality.as_str() {
            "EXACT" => s.exact_jobs += 1,
            "ESTIMATED" => s.estimated_jobs += 1,
            "MIXED" => s.mixed_jobs += 1,
            "PARTIAL" => s.partial_jobs += 1,
            _ => s.unknown_jobs += 1,
        }
        if j.role.eq_ignore_ascii_case("verifier") {
            match j
                .reported_decision
                .as_deref()
                .or(j.canonical_decision.as_deref())
            {
                Some("PASS") => s.pass += 1,
                Some("REJECT") => s.reject += 1,
                _ => s.decisions_unknown += 1,
            }
        }
        if j.executor_disposition.as_deref() == Some("PASS")
            || j.canonical_decision.as_deref() == Some("PASS")
        {
            if let Some(t) = &j.task {
                verified.insert((&j.workspace, &j.plan, t));
            }
        }
        if let Some(round) = j.correction_round {
            *s.correction_rounds.entry(round).or_default() += 1;
            s.correction_usage
                .entry(round)
                .or_default()
                .merge(&j.tokens);
            s.correction_jobs += usize::from(round > 0);
        }
        match j.route_attempt {
            Some(0) => {
                s.route_resolutions += 1;
                s.policy_skipped += j.policy_skipped.unwrap_or(0);
            }
            Some(n) => {
                s.fallback_attempts += 1;
                s.fallback_executions += usize::from(j.started_ms.is_some());
                *s.fallback_depth.entry(n).or_default() += 1;
            }
            None => s.route_unknown += 1,
        }
        if let Some(reason) = &j.availability_failure {
            *s.availability_failures.entry(reason.clone()).or_default() += 1;
        }
    }
    s.associated_verified_tasks = verified.len();
    s.reject_rate = Rate::new(s.reject, s.pass + s.reject);
    // Attempt-level rate: fallback starts / all started jobs with route provenance.
    s.fallback_rate = Rate::new(
        s.fallback_executions,
        jobs.iter()
            .filter(|j| j.started_ms.is_some() && j.route_attempt.is_some())
            .count(),
    );
    s.prompt_bytes = Distribution::of(jobs.iter().map(|j| j.prompt_bytes));
    s.context_bytes = Distribution::of(jobs.iter().map(|j| j.context_bytes));
    s.truncation_rate = Rate::new(
        jobs.iter()
            .filter(|j| j.context_truncated == Some(true))
            .count(),
        jobs.iter()
            .filter(|j| j.context_truncated.is_some())
            .count(),
    );
    s.truncation_unknown = jobs.len() - s.truncation_rate.denominator;
    s.completed_execution_ms = Distribution::of(jobs.iter().map(|j| j.completed_execution_ms));
    s.wall_elapsed_ms = Distribution::of(jobs.iter().map(|j| j.wall_elapsed_ms));
    s.token_quality = s.tokens.total.quality().into();
    s
}
pub(crate) fn groups(jobs: &[Job], dimension: &str) -> Vec<Group> {
    let mut groups: BTreeMap<_, Vec<&Job>> = BTreeMap::new();
    for j in jobs {
        groups
            .entry((
                if ["roles", "routes", "role_providers"].contains(&dimension) {
                    Some(j.role.clone())
                } else {
                    None
                },
                if dimension != "roles" {
                    j.provider.clone()
                } else {
                    None
                },
                if ["routes", "models"].contains(&dimension) {
                    j.model.clone()
                } else {
                    None
                },
            ))
            .or_default()
            .push(j);
    }
    groups
        .into_iter()
        .map(|((role, provider, model), jobs)| Group {
            role,
            provider,
            model,
            summary: summarize(&jobs),
        })
        .collect()
}
