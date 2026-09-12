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

pub fn cli(paths: &MachinePaths, args: &[&str]) -> Result<()> {
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
        ["usage"] => serde_json::to_value(usage::series(&snapshot, usage::Scope::Aggregate, None))?,
        ["usage", kind, value] => {
            let scope = match *kind { "provider"=>usage::Scope::Provider((*value).into()),"task"=>usage::Scope::Task((*value).into()),"role"=>usage::Scope::Role((*value).into()),_=>return Err(Error::Invalid("usage filter must be provider, task or role".into())) };
            serde_json::to_value(usage::series(&snapshot, scope, None))?
        }
        _ => return Err(Error::Invalid("observe snapshot|sessions|session ID|agents|agent ID|job ID|tasks|task ID|events|usage [provider|task|role VALUE] [--json]".into())),
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
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
