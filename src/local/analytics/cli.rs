use super::*;
use crate::local::{
    self, Error, Result,
    paths::MachinePaths,
    repository::{RepositoryId, RepositoryInfo},
    store::Store,
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
    let store = Store::read_only(&paths.database, 5000)?;
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
    // An unregistered repository has no cohort at all. Reporting an empty one
    // would be indistinguishable from a registered repository that has simply
    // not run anything, so both the discovered and the named repository are
    // checked — `--repository` is the route the message above recommends.
    local::require(
        RepositoryId::try_from(repository.clone())
            .ok()
            .and_then(|id| store.repository(&id).transpose())
            .transpose()?
            .is_some(),
        format!(
            "repository {repository} is not registered with agentctl; run agentctl repo init in that checkout, or name a registered repository with --repository"
        ),
    )?;
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
    let snapshot = store.analytics(q, now)?;
    if json_mode {
        println!("{}", crate::local::terminal::json(&snapshot)?);
        return Ok(());
    }
    use crate::local::terminal::{Report, name};
    // Absent values are UNKNOWN or not applicable, never zero.
    fn known<T: std::fmt::Display>(value: &Option<T>) -> String {
        value
            .as_ref()
            .map_or_else(|| "UNKNOWN".into(), ToString::to_string)
    }
    let s = &snapshot.summary;
    let mut report = Report::new(format!("Analytics — {command}"));
    report
        .field(
            "Jobs",
            format!("{} ({} contract-completed)", s.jobs, s.contract_completed),
        )
        .field("Usage", &s.token_quality)
        .field(
            "Tokens",
            format!(
                "{} exact, {} estimated",
                known(&s.tokens.total.exact),
                known(&s.tokens.total.estimated)
            ),
        )
        .field(
            "Incomplete",
            format!(
                "{} unknown, {} partial job(s)",
                s.unknown_jobs, s.partial_jobs
            ),
        )
        .field(
            "Rejections",
            format!(
                "{}/{} verifier REJECT",
                s.reject_rate.numerator, s.reject_rate.denominator
            ),
        )
        .field(
            "Fallbacks",
            format!(
                "{}/{} executions; {} policy skip(s)",
                s.fallback_rate.numerator, s.fallback_rate.denominator, s.policy_skipped
            ),
        )
        .field(
            "Coverage",
            format!(
                "{} complete, {} partial (VERIFIED task usage)",
                snapshot.verified_tasks_with_complete_usage,
                snapshot.verified_tasks_with_partial_usage
            ),
        )
        .field(
            "Context",
            format!("median {} bytes", known(&s.prompt_bytes.median)),
        )
        .field(
            "Duration",
            format!(
                "median {} ms (completed executions)",
                known(&s.completed_execution_ms.median)
            ),
        );
    if !snapshot.sessions.is_empty() {
        report.section("Sessions");
        for session in snapshot.sessions.iter().take(20) {
            report.field(
                &session.id,
                format!(
                    "{}  {}/{} VERIFIED  correction round {}",
                    session.lifecycle,
                    session.verified,
                    session.current_tasks,
                    known(&session.correction_round)
                ),
            );
        }
    }
    if (command == "task" || command == "corrections") && !snapshot.tasks.is_empty() {
        report.section("Tasks");
        for task in snapshot.tasks.iter().take(20) {
            report.field(
                &task.id,
                format!(
                    "{}  attempts {}/{} (executor/verifier)  corrections {}  round {}  usage {}",
                    task.lifecycle,
                    task.executor_jobs.len(),
                    task.verifier_jobs.len(),
                    known(&task.correction_count),
                    known(&task.correction_round),
                    task.summary.token_quality
                ),
            );
        }
    }
    if command == "job" && !snapshot.jobs.is_empty() {
        report.section("Jobs");
        for job in &snapshot.jobs {
            report.field(
                &job.id,
                format!(
                    "{}  {}/{}  {}  fallback depth {}  usage {}  wall {} ms",
                    job.role,
                    job.provider.as_deref().unwrap_or("UNKNOWN"),
                    job.model.as_deref().unwrap_or("UNKNOWN"),
                    job.lifecycle,
                    known(&job.route_attempt),
                    job.usage_quality,
                    known(&job.wall_elapsed_ms)
                ),
            );
        }
    }
    if command == "routes" || command == "job" {
        let routed: Vec<_> = snapshot
            .jobs
            .iter()
            .take(20)
            .filter_map(|job| job.route_provenance.as_ref().map(|r| (job, r)))
            .collect();
        if !routed.is_empty() {
            report.section("Route provenance");
            for (job, route) in routed {
                let preceding: Vec<String> = route
                    .preceding_failures
                    .iter()
                    .map(|f| name(&f.reason))
                    .collect();
                report.field(
                    &job.id,
                    format!(
                        "provider {}  model {}  fallback {}  depth {}  preceding [{}]  policy skips {}",
                        route.provider_source.as_deref().unwrap_or("UNKNOWN"),
                        route.model_source.as_deref().unwrap_or("UNKNOWN"),
                        route.fallback_source.as_deref().unwrap_or("UNKNOWN"),
                        known(&route.fallback_depth),
                        preceding.join(", "),
                        route.policy_skipped.len()
                    ),
                );
            }
        }
    }
    let groups = if command == "routes" {
        &snapshot.routes
    } else {
        &snapshot.roles
    };
    if !groups.is_empty() {
        report.section(if command == "routes" {
            "By route"
        } else {
            "By role"
        });
        for g in groups.iter().take(20) {
            report.field(
                &format!(
                    "{}/{}/{}",
                    g.role.as_deref().unwrap_or("UNKNOWN"),
                    g.provider.as_deref().unwrap_or("UNKNOWN"),
                    g.model.as_deref().unwrap_or("UNKNOWN")
                ),
                format!(
                    "{} job(s)  usage {}  {} exact, {} estimated tokens",
                    g.summary.jobs,
                    g.summary.token_quality,
                    known(&g.summary.tokens.total.exact),
                    known(&g.summary.tokens.total.estimated)
                ),
            );
        }
    }
    report.section("Notes");
    for warning in &snapshot.warnings {
        report.text(format!("- {warning}"));
    }
    report.text(format!(
        "- Lists show at most 20 entries; agentctl analytics {command} --json has the full bounded snapshot."
    ));
    // Provider/model/role names and warnings are untrusted text.
    println!("{}", crate::local::terminal::human(&report.to_string()));
    Ok(())
}
