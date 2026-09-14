use super::*;
use crate::local::{
    self, Error, Result, paths::MachinePaths, repository::RepositoryInfo, store::Store,
};
pub(crate) fn run(paths: &MachinePaths, args: &[&str], json_mode: bool) -> Result<()> {
    let now = local::now_ms()?;
    let mut positional = vec![];
    let mut flags = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with("--") {
            let key = args[i];
            let value = args
                .get(i + 1)
                .ok_or_else(|| Error::Invalid(format!("{key} needs a value")))?;
            local::require(
                flags.insert(key, *value).is_none(),
                "duplicate analytics filter",
            )?;
            i += 2;
        } else {
            positional.push(args[i]);
            i += 1;
        }
    }
    let (repository, workspace) = if let Some(repo) = flags.get("--repository") {
        (
            (*repo).to_owned(),
            flags.get("--workspace").map(|s| (*s).to_owned()),
        )
    } else {
        let info = RepositoryInfo::discover(&std::env::current_dir()?)?;
        (
            info.repository_id.as_str().into(),
            Some(
                flags
                    .get("--workspace")
                    .map(|s| (*s).to_owned())
                    .unwrap_or_else(|| info.workspace_id.as_str().into()),
            ),
        )
    };
    let mut q = Query::workspace(repository, workspace.clone().unwrap_or_default(), now);
    q.workspace = workspace;
    for (k, v) in flags {
        match k {
            "--repository" | "--workspace" => {}
            "--session" => q.session = Some(v.into()),
            "--role" => q.role = Some(v.into()),
            "--provider" => q.provider = Some(v.into()),
            "--model" => q.model = Some(v.into()),
            "--task" => q.task = Some(v.into()),
            "--job" => q.job = Some(v.into()),
            "--lifecycle" => q.lifecycle = Some(v.into()),
            "--from-ms" => {
                q.from_ms = v
                    .parse()
                    .map_err(|_| Error::Invalid("invalid --from-ms".into()))?
            }
            "--to-ms" => {
                q.to_ms = v
                    .parse()
                    .map_err(|_| Error::Invalid("invalid --to-ms".into()))?
            }
            "--limit" => {
                q.limit = v
                    .parse()
                    .map_err(|_| Error::Invalid("invalid --limit".into()))?
            }
            _ => return Err(Error::Invalid(format!("unknown analytics filter {k}"))),
        }
    }
    let command = match positional.as_slice() {
        [] => "summary",
        [name] if ["summary", "usage", "roles", "routes", "corrections"].contains(name) => name,
        ["session", id] => {
            q.session = Some((*id).into());
            "session"
        }
        ["task", id] => {
            q.task = Some((*id).into());
            "task"
        }
        ["job" | "agent", id] => {
            q.job = Some((*id).into());
            "job"
        }
        _ => return Err(Error::Invalid(
            "expected analytics summary|usage|roles|routes|corrections|session ID|task ID|job ID"
                .into(),
        )),
    };
    let snapshot = Store::read_only(&paths.database, 100)?.analytics(q, now)?;
    if json_mode {
        println!("{}", crate::local::terminal::json(&snapshot)?);
        return Ok(());
    }
    // Provider/model/role names and warnings are untrusted text.
    macro_rules! say {
        ($($arg:tt)*) => {
            println!("{}", crate::local::terminal::human(&format!($($arg)*)))
        };
    }
    let s = &snapshot.summary;
    say!(
        "ANALYTICS v{} — {command}\nJobs {} | contract-completed {} | usage {}\nTokens exact {:?} | estimated {:?} | unknown jobs {} | partial jobs {}\nVerifier REJECT {}/{} | fallback executions {}/{} | policy skips {}\nVERIFIED packet usage coverage {} complete / {} partial\nContext bytes median {:?} | completed execution ms median {:?}",
        snapshot.analytics_version,
        s.jobs,
        s.contract_completed,
        s.token_quality,
        s.tokens.total.exact,
        s.tokens.total.estimated,
        s.unknown_jobs,
        s.partial_jobs,
        s.reject_rate.numerator,
        s.reject_rate.denominator,
        s.fallback_rate.numerator,
        s.fallback_rate.denominator,
        s.policy_skipped,
        snapshot.verified_tasks_with_complete_usage,
        snapshot.verified_tasks_with_partial_usage,
        s.prompt_bytes.median,
        s.completed_execution_ms.median
    );
    for session in snapshot.sessions.iter().take(20) {
        say!(
            "Session {}: {} | {}/{} VERIFIED | correction round {:?}",
            session.id,
            session.lifecycle,
            session.verified,
            session.current_tasks,
            session.correction_round
        );
    }
    if command == "task" || command == "corrections" {
        for task in snapshot.tasks.iter().take(20) {
            say!(
                "Task {}: {} | executor/verifier attempts {}/{} | correction count {:?}, round {:?} | usage {}",
                task.id,
                task.lifecycle,
                task.executor_jobs.len(),
                task.verifier_jobs.len(),
                task.correction_count,
                task.correction_round,
                task.summary.token_quality
            );
        }
    }
    if command == "job" {
        for job in &snapshot.jobs {
            say!(
                "Job {}: {} / {} / {} | {} | fallback depth {:?} | usage {} | wall ms {:?}",
                job.id,
                job.role,
                job.provider.as_deref().unwrap_or("UNKNOWN"),
                job.model.as_deref().unwrap_or("UNKNOWN"),
                job.lifecycle,
                job.route_attempt,
                job.usage_quality,
                job.wall_elapsed_ms
            );
        }
    }
    say!(
        "Human lists show at most 20 entries; --json exposes the full bounded snapshot. None means UNKNOWN/not applicable, never zero."
    );
    if command == "routes" || command == "job" {
        for job in snapshot.jobs.iter().take(20) {
            if let Some(route) = &job.route_provenance {
                say!(
                    "Route {}: provider={} model={} fallback-source={} depth={:?} preceding={:?} policy-skips={}",
                    job.id,
                    route.provider_source.as_deref().unwrap_or("UNKNOWN"),
                    route.model_source.as_deref().unwrap_or("UNKNOWN"),
                    route.fallback_source.as_deref().unwrap_or("UNKNOWN"),
                    route.fallback_depth,
                    route
                        .preceding_failures
                        .iter()
                        .map(|f| f.reason)
                        .collect::<Vec<_>>(),
                    route.policy_skipped.len()
                );
            }
        }
    }
    let groups = if command == "routes" {
        &snapshot.routes
    } else {
        &snapshot.roles
    };
    for g in groups.iter().take(20) {
        say!(
            "{} / {} / {}: {} jobs, {} usage, exact {:?}, estimated {:?}",
            g.role.as_deref().unwrap_or("UNKNOWN"),
            g.provider.as_deref().unwrap_or("UNKNOWN"),
            g.model.as_deref().unwrap_or("UNKNOWN"),
            g.summary.jobs,
            g.summary.token_quality,
            g.summary.tokens.total.exact,
            g.summary.tokens.total.estimated
        );
    }
    for warning in &snapshot.warnings {
        say!("Note: {warning}");
    }
    Ok(())
}
