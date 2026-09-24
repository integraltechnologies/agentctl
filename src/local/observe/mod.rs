//! Bounded, read-only semantic projections. No provider calls, filesystem discovery,
//! authority grants, lifecycle mutations, transcript reads or timer writes.
mod model;
mod query;
pub mod usage;
use super::{Error, Result, paths::MachinePaths, store::Store};
pub use model::*;
use serde::{Deserialize, Serialize};

pub fn read(paths: &MachinePaths, at_ms: u64) -> Result<Snapshot> {
    if !paths.database.exists() {
        return Ok(Snapshot {
            at_ms,
            warnings: vec!["No local database; initialize agentctl to register workspaces.".into()],
            ..Default::default()
        });
    }
    Store::read_only(&paths.database, 100)?.observe(at_ms)
}

pub fn cli(paths: &MachinePaths, args: &[&str], json_mode: bool) -> Result<()> {
    let snapshot = read(paths, super::now_ms()?)?;
    let value = match args {
        [] | ["snapshot"] => serde_json::to_value(&snapshot)?,
        ["sessions"] => serde_json::to_value(&snapshot.sessions)?,
        ["agents"] => serde_json::to_value(&snapshot.agents)?,
        ["tasks"] => serde_json::to_value(&snapshot.tasks)?,
        ["events"] => serde_json::to_value(snapshot.events.iter().rev().take(50).collect::<Vec<_>>())?,
        ["session", id] => {
            let session = snapshot.sessions.iter().find(|s| s.id == *id).ok_or_else(|| Error::Invalid("session not found in bounded recent view".into()))?;
            serde_json::json!({"session":session,"agents":snapshot.agents.iter().filter(|a|a.session_id.as_deref()==Some(id)).collect::<Vec<_>>(),"tasks":snapshot.tasks.iter().filter(|t|t.session_id.as_deref()==Some(id)).collect::<Vec<_>>()})
        }
        ["agent", id] | ["job", id] => serde_json::to_value(snapshot.agents.iter().find(|a| a.id == *id || a.job_id == *id).ok_or_else(||Error::Invalid("agent/job not found in bounded recent view".into()))?)?,
        ["task", id] => {
            let matches: Vec<_> = snapshot.tasks.iter().filter(|t|t.id == *id).collect();
            super::require(!matches.is_empty(), "task not found in bounded recent view")?;
            serde_json::to_value(matches)?
        }
        ["experiments"] => serde_json::to_value(&snapshot.experiments)?,
        ["experiment", id] => serde_json::to_value(
            snapshot
                .experiments
                .iter()
                .find(|e| e.id == *id)
                .ok_or_else(|| Error::Invalid("experiment not found in bounded recent view".into()))?,
        )?,
        ["usage"] => serde_json::to_value(usage::series(&snapshot, usage::Scope::Aggregate, None))?,
        ["usage", kind, value] => {
            let scope = match *kind { "provider"=>usage::Scope::Provider((*value).into()),"task"=>usage::Scope::Task((*value).into()),"role"=>usage::Scope::Role((*value).into()),_=>return Err(Error::Invalid("usage filter must be provider, task or role".into())) };
            serde_json::to_value(usage::series(&snapshot, scope, None))?
        }
        _ => return Err(Error::Invalid("observe snapshot|sessions|session ID|agents|agent ID|job ID|tasks|task ID|events|experiments|experiment ID|usage [provider|task|role VALUE] [--json]".into())),
    };
    if json_mode {
        println!("{}", crate::local::terminal::json_compact(&value)?);
    } else {
        println!(
            "{}",
            crate::local::terminal::human(&human(&snapshot, args, &value))
        );
    }
    Ok(())
}

/// Human rendering of an `observe` view. Lists are one line per item; single
/// items are field reports. `--json` carries the complete projection.
fn human(snapshot: &Snapshot, args: &[&str], value: &serde_json::Value) -> String {
    use crate::local::terminal::Report;
    let session_line = |s: &Session| {
        format!(
            "{:<10}  {}/{} VERIFIED  {}",
            s.state, s.verified, s.task_count, s.title
        )
    };
    let agent_line = |a: &Agent| {
        format!(
            "{:<9}  {:<10}  {}  {}",
            a.role,
            a.state,
            a.provider.as_deref().unwrap_or("-"),
            a.task_id.as_deref().unwrap_or("")
        )
    };
    let task_line = |t: &Task| format!("{:<10}  {}", t.presentation, t.objective);
    let experiment_line = |e: &Experiment| {
        format!(
            "{:<10}  attempt {}  {}",
            e.state, e.attempt, e.command_summary
        )
    };
    let event_line = |e: &Event| format!("{:>6}  {:<22}  {}", e.sequence, e.phase, e.summary);
    let list = |title: &str, rows: Vec<(String, String)>| {
        let mut report = Report::new(format!("{title} ({})", rows.len()));
        if rows.is_empty() {
            report.text("none in the recent view");
        }
        for (id, line) in rows {
            report.field(&id, line);
        }
        report.to_string()
    };
    let blocker = |report: &mut Report, b: &Option<Blocker>| {
        if let Some(b) = b {
            report.field("Blocker", format!("{}: {}", b.kind, b.description));
        }
    };
    match args {
        [] | ["snapshot"] => {
            let mut report = Report::new("Snapshot");
            report
                .field("Sessions", snapshot.sessions.len())
                .field("Agents", snapshot.agents.len())
                .field("Tasks", snapshot.tasks.len())
                .field("Experiments", snapshot.experiments.len())
                .field("Events", snapshot.events.len());
            if snapshot.truncated {
                report.field("View", "TRUNCATED (bounded recent view)");
            }
            if !snapshot.sessions.is_empty() {
                report.section("Sessions");
                for s in &snapshot.sessions {
                    report.field(&s.id, session_line(s));
                }
            }
            if !snapshot.warnings.is_empty() {
                report.section("Warnings");
                for w in &snapshot.warnings {
                    report.text(w.clone());
                }
            }
            report.text("").text("Live view: agenttop");
            report.to_string()
        }
        ["sessions"] => list(
            "Sessions",
            snapshot
                .sessions
                .iter()
                .map(|s| (s.id.clone(), session_line(s)))
                .collect(),
        ),
        ["agents"] => list(
            "Agents",
            snapshot
                .agents
                .iter()
                .map(|a| (a.id.clone(), agent_line(a)))
                .collect(),
        ),
        ["tasks"] => list(
            "Tasks",
            snapshot
                .tasks
                .iter()
                .map(|t| (t.id.clone(), task_line(t)))
                .collect(),
        ),
        ["experiments"] => list(
            "Experiments",
            snapshot
                .experiments
                .iter()
                .map(|e| (e.id.clone(), experiment_line(e)))
                .collect(),
        ),
        ["events"] => list(
            "Events",
            snapshot
                .events
                .iter()
                .rev()
                .take(50)
                .map(|e| (e.at_ms.to_string(), event_line(e)))
                .collect(),
        ),
        ["session", id] => {
            let Some(s) = snapshot.sessions.iter().find(|s| s.id == *id) else {
                return String::new();
            };
            let mut report = Report::new(format!("Session {}", s.id));
            report
                .field("Status", &s.state)
                .field("Title", &s.title)
                .field(
                    "Progress",
                    format!("{}/{} VERIFIED", s.verified, s.task_count),
                )
                .field_opt("Plan", s.current_plan.as_deref())
                .field_opt(
                    "Correction",
                    s.correction_round.map(|r| format!("round {r}")),
                )
                .field("Activity", &s.activity)
                .field("Root", &s.root);
            blocker(&mut report, &s.blocker);
            report.section("Agents");
            for a in snapshot
                .agents
                .iter()
                .filter(|a| a.session_id.as_deref() == Some(id))
            {
                report.field(&a.id, agent_line(a));
            }
            report.section("Tasks");
            for t in snapshot
                .tasks
                .iter()
                .filter(|t| t.session_id.as_deref() == Some(id))
            {
                report.field(&t.id, task_line(t));
            }
            report.to_string()
        }
        ["agent", id] | ["job", id] => {
            let Some(a) = snapshot
                .agents
                .iter()
                .find(|a| a.id == *id || a.job_id == *id)
            else {
                return String::new();
            };
            let mut report = Report::new(format!("Agent {}", a.id));
            report
                .field("Job", &a.job_id)
                .field("Role", &a.role)
                .field("Status", &a.state)
                .field("Liveness", a.liveness.as_str())
                .field(
                    "Provider",
                    format!(
                        "{}/{}",
                        a.provider.as_deref().unwrap_or("UNKNOWN"),
                        a.model.as_deref().unwrap_or("UNKNOWN")
                    ),
                )
                .field_opt("Plan", a.plan_id.as_deref())
                .field_opt("Task", a.task_id.as_deref())
                .field_opt("Verification", a.verification.as_deref())
                .field_opt("Fallback", a.fallback_reason.as_deref())
                .field("Activity", &a.activity);
            blocker(&mut report, &a.blocker);
            report.to_string()
        }
        ["task", id] => {
            let mut report = Report::default();
            for t in snapshot.tasks.iter().filter(|t| t.id == *id) {
                report
                    .section(format!("Task {}", t.id))
                    .field("Plan", &t.plan_id)
                    .field("Status", &t.lifecycle)
                    .field("Objective", &t.objective)
                    .field_opt("Executor", t.executor_job.as_deref())
                    .field_opt("Verifier", t.verifier_job.as_deref());
                blocker(&mut report, &t.blocker);
            }
            report.to_string()
        }
        ["experiment", id] => {
            let Some(e) = snapshot.experiments.iter().find(|e| e.id == *id) else {
                return String::new();
            };
            Report::new(format!("Experiment {}", e.id))
                .field("Status", &e.state)
                .field("Liveness", e.liveness.as_str())
                .field("Attempt", e.attempt)
                .field("Command", &e.command_summary)
                .field_opt("Exit", e.exit_status)
                .field("Cancel", crate::local::terminal::yes_no(e.cancel_requested))
                .to_string()
        }
        // Usage series are rate data for agenttop; show them as compact JSON.
        _ => crate::local::terminal::json_compact(value).unwrap_or_default(),
    }
}

/// Depth-first stable presentation ordering. Unknown parents/cycles stay visible;
/// no ownership is repaired or inferred across sessions.
pub fn tree(snapshot: &Snapshot, session: Option<&str>) -> Vec<(usize, usize)> {
    use std::collections::BTreeSet;
    let mut remaining: BTreeSet<usize> = snapshot
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.session_id.as_deref() == session)
        .map(|(i, _)| i)
        .collect();
    let mut output = vec![];
    while !remaining.is_empty() {
        let root = remaining
            .iter()
            .copied()
            .find(|i| {
                !remaining.iter().any(|j| {
                    snapshot.agents[*i].parent_id.as_deref()
                        == Some(snapshot.agents[*j].id.as_str())
                })
            })
            .unwrap_or_else(|| *remaining.first().expect("nonempty"));
        let mut stack = vec![(root, 0)];
        while let Some((i, depth)) = stack.pop() {
            if !remaining.remove(&i) {
                continue;
            }
            output.push((i, depth));
            for child in remaining.iter().rev().filter(|j| {
                snapshot.agents[**j].parent_id.as_deref() == Some(snapshot.agents[i].id.as_str())
            }) {
                stack.push((*child, (depth + 1).min(32)));
            }
        }
    }
    output
}
