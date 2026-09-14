use super::*;
use experiment::{ExperimentInput, ExperimentObservation, ExperimentRun, ExperimentRuntime};
use serde_json::json;

fn value(args: &[&str], i: &mut usize) -> Result<String> {
    let flag = args[*i];
    let v = args
        .get(*i + 1)
        .ok_or_else(|| Error::Invalid(format!("{flag} needs a value")))?
        .to_string();
    *i += 2;
    Ok(v)
}

/// `ID:METRIC:OP:VALUE:record[:TAG=VAL,...]` or
/// `ID:METRIC:OP:VALUE:planner:VERIFICATION_REF[:TAG=VAL,...]`. A deliberately tiny
/// grammar: one scalar comparison, one required-match tag selector, one of two
/// actions. No expression language, no scripts. Omitting the tag segment leaves the
/// selector empty, which (see `local::runtime::experiment_decisions::tags_match`)
/// fail-closed matches only an untagged metric of that name - it is never a wildcard.
fn parse_boundary(spec: &str) -> Result<BoundaryDefinition> {
    const USAGE: &str = "--boundary ID:METRIC:OP:VALUE:record[:TAG=VAL,...] or ID:METRIC:OP:VALUE:planner:VERIFICATION_REF[:TAG=VAL,...] (OP is one of < <= > >= ==)";
    let mut parts = spec.splitn(5, ':');
    let (Some(boundary_id), Some(metric), Some(op), Some(value), Some(rest)) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(Error::Invalid(USAGE.into()));
    };
    let comparison = match op {
        "<" => MetricComparison::LessThan,
        "<=" => MetricComparison::LessThanOrEqual,
        ">" => MetricComparison::GreaterThan,
        ">=" => MetricComparison::GreaterThanOrEqual,
        "==" => MetricComparison::Equal,
        other => {
            return Err(Error::Invalid(format!(
                "--boundary: unknown comparison {other}; use <, <=, >, >=, or =="
            )));
        }
    };
    let value: f64 = value
        .parse()
        .map_err(|_| Error::Invalid("--boundary: VALUE must be a finite number".into()))?;
    require(value.is_finite(), "--boundary: VALUE must be finite")?;
    let mut rest = rest.splitn(3, ':');
    let action_word = rest.next().ok_or_else(|| Error::Invalid(USAGE.into()))?;
    let (verification_ref, tag_spec) = match action_word {
        "record" => (None, rest.next()),
        "planner" => {
            let verification_ref = rest.next().ok_or_else(|| Error::Invalid(USAGE.into()))?;
            (Some(verification_ref), rest.next())
        }
        other => {
            return Err(Error::Invalid(format!(
                "--boundary: unknown action {other}; use record or planner:VERIFICATION_REF"
            )));
        }
    };
    require(rest.next().is_none(), USAGE)?;
    let action = match verification_ref {
        None => BoundaryAction::RecordOnly,
        Some(verification_ref) => BoundaryAction::RequirePlannerReview {
            verification_ref: verification_ref.to_string(),
        },
    };
    Ok(BoundaryDefinition {
        boundary_id: boundary_id.to_string(),
        condition: ExperimentBoundary::MetricThreshold {
            metric: metric.to_string(),
            tags: parse_tags(tag_spec)?,
            comparison,
            value,
        },
        action,
    })
}

fn parse_tags(spec: Option<&str>) -> Result<BTreeMap<String, String>> {
    let mut tags = BTreeMap::new();
    let Some(spec) = spec else {
        return Ok(tags);
    };
    for pair in spec.split(',') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| Error::Invalid(format!("--boundary tag {pair:?} must be KEY=VALUE")))?;
        require(
            !key.is_empty() && !value.is_empty(),
            "--boundary tag KEY and VALUE must be nonempty",
        )?;
        require(
            tags.insert(key.to_string(), value.to_string()).is_none(),
            format!("--boundary: duplicate tag key {key}"),
        )?;
    }
    Ok(tags)
}

fn human_run(run: &ExperimentRun) -> String {
    let attempt = run.attempts.last();
    format!(
        "{}  {:?}  {} {:?}  attempts={}  pid={:?}  exit={:?}",
        run.experiment_id.as_str(),
        run.state,
        run.command.program,
        run.command.args,
        run.attempts.len(),
        attempt.and_then(|a| a.pid),
        attempt.and_then(|a| a.exit_status)
    )
}
fn human_observation(o: &ExperimentObservation) -> String {
    format!(
        "{}  liveness={:?}  events={}  ingestion_errors={}  decisions={}  wakeups={}/{}  attention_required={}{}",
        human_run(&o.run),
        o.liveness,
        o.events.event_count,
        o.events.ingestion_errors,
        o.control.decision_count,
        o.control.wakeups_created,
        o.control.wakeups_budget,
        o.control.attention_required,
        if o.events.event_volume_capped {
            "  EVENT_VOLUME_CAP_EXCEEDED (later events not recorded)"
        } else {
            ""
        }
    )
}

fn event_query(
    args: &[&str],
    event_type: Option<&str>,
    allow_name: bool,
) -> Result<(ExperimentId, ExperimentEventQuery)> {
    let id = args
        .first()
        .ok_or_else(|| Error::Invalid("experiment event query requires an ID".into()))?;
    let id = ExperimentId::new(*id).map_err(Error::Invalid)?;
    let mut query = ExperimentEventQuery {
        event_type: event_type.map(str::to_owned),
        ..Default::default()
    };
    let mut i = 1;
    while i < args.len() {
        match args[i] {
            "--attempt" => {
                let v = value(args, &mut i)?;
                query.attempt = Some(
                    v.parse()
                        .map_err(|_| Error::Invalid("invalid --attempt".into()))?,
                );
            }
            "--name" if allow_name => query.metric_name = Some(value(args, &mut i)?),
            "--limit" => {
                let v = value(args, &mut i)?;
                query.limit = v
                    .parse()
                    .map_err(|_| Error::Invalid("invalid --limit".into()))?;
            }
            other => {
                return Err(Error::Invalid(format!(
                    "unknown experiment event query flag {other}"
                )));
            }
        }
    }
    require(query.attempt != Some(0), "--attempt must be at least 1")?;
    Ok((id, query))
}

fn human_event(event: &ExperimentRuntimeEvent) -> String {
    let fact = match &event.event {
        ExperimentEventData::Metric {
            name,
            value,
            step,
            epoch,
            ..
        } => {
            format!("metric {name}={value} step={step:?} epoch={epoch:?}")
        }
        ExperimentEventData::Checkpoint {
            name,
            path,
            byte_size,
            ..
        } => {
            format!("checkpoint {name} path={path} bytes={byte_size}")
        }
        ExperimentEventData::Health { kind, message } => {
            format!("health {kind:?} {}", message.as_deref().unwrap_or(""))
        }
        ExperimentEventData::ProcessStatus { status, message } => {
            format!("status {status} {}", message.as_deref().unwrap_or(""))
        }
    };
    format!(
        "{}  attempt={} source_seq={} timestamp_ms={} source={}  {fact}",
        event.arrival_sequence,
        event.attempt,
        event.source_sequence,
        event.timestamp_ms,
        event.source
    )
}

pub(crate) fn run(
    store: &mut Store,
    paths: &paths::MachinePaths,
    security: &crate::local::security::SecurityConfig,
    command: &str,
    args: &[&str],
    json_mode: bool,
) -> Result<()> {
    let root = std::env::current_dir()?;
    match command {
        "run" => {
            let mut program: Option<String> = None;
            let mut command_key: Option<String> = None;
            let mut cwd: Option<String> = None;
            let mut argv: Vec<String> = vec![];
            let mut network = false;
            let mut env_passthrough: Vec<String> = vec![];
            let mut timeout_ms = EXPERIMENT_DEFAULT_TIMEOUT_MS;
            let mut decision_boundaries: Vec<BoundaryDefinition> = vec![];
            let mut max_planner_wakeups = DEFAULT_MAX_PLANNER_WAKEUPS;
            let mut i = 0;
            while i < args.len() {
                match args[i] {
                    "--program" => program = Some(value(args, &mut i)?),
                    "--command" => command_key = Some(value(args, &mut i)?),
                    "--cwd" => cwd = Some(value(args, &mut i)?),
                    "--arg" => argv.push(value(args, &mut i)?),
                    "--env" => env_passthrough.push(value(args, &mut i)?),
                    "--timeout-ms" => {
                        let v = value(args, &mut i)?;
                        timeout_ms = v
                            .parse()
                            .map_err(|_| Error::Invalid("invalid --timeout-ms".into()))?;
                    }
                    "--boundary" => decision_boundaries.push(parse_boundary(&value(args, &mut i)?)?),
                    "--max-wakeups" => {
                        let v = value(args, &mut i)?;
                        max_planner_wakeups = v
                            .parse()
                            .map_err(|_| Error::Invalid("invalid --max-wakeups".into()))?;
                    }
                    "--network" => {
                        network = true;
                        i += 1;
                    }
                    other => {
                        return Err(Error::Invalid(format!(
                            "unknown experiment run flag {other}"
                        )));
                    }
                }
            }
            require(
                program.is_some() != command_key.is_some(),
                "experiment run requires exactly one of --program or --command",
            )?;
            let project_command = if command_key.is_some() {
                require(
                    argv.is_empty() && cwd.is_none(),
                    "--command uses the project-defined [commands.KEY] as-is; it cannot be combined with --arg/--cwd",
                )?;
                command_key
            } else {
                None
            };
            let command = program.map(|program| CommandSpec {
                program,
                args: argv,
                cwd: cwd.unwrap_or_else(|| ".".into()),
            });
            let mut runtime =
                ExperimentRuntime::new(store, paths.clone())?.with_security(security.clone())?;
            let run = if let Some(key) = project_command {
                runtime.run_project_command(
                    &root,
                    key,
                    network,
                    env_passthrough,
                    timeout_ms,
                    decision_boundaries,
                    max_planner_wakeups,
                )?
            } else {
                runtime.run(
                    &root,
                    ExperimentInput {
                        command: command.expect("exactly one command source was checked"),
                        network,
                        env_passthrough,
                        timeout_ms,
                        decision_boundaries,
                        max_planner_wakeups,
                    },
                )?
            };
            output(json_mode, &run, &human_run(&run))
        }
        "restart" => {
            require(
                args.len() == 1,
                "experiment restart requires exactly one ID",
            )?;
            let id = ExperimentId::new(args[0]).map_err(Error::Invalid)?;
            let mut runtime =
                ExperimentRuntime::new(store, paths.clone())?.with_security(security.clone())?;
            let run = runtime.restart(&root, &id)?;
            output(json_mode, &run, &human_run(&run))
        }
        "cancel" => {
            require(args.len() == 1, "experiment cancel requires exactly one ID")?;
            let id = ExperimentId::new(args[0]).map_err(Error::Invalid)?;
            store.experiment_cancel(&root, &id)?;
            output(
                json_mode,
                &json!({"experiment_id": id.as_str(), "cancellation_requested": true}),
                "Cancellation requested; effective only if a live controller is currently polling this experiment. A crashed controller cannot observe this and nothing is falsely reported as killed.",
            )
        }
        "status" => {
            require(args.len() == 1, "experiment status requires exactly one ID")?;
            let id = ExperimentId::new(args[0]).map_err(Error::Invalid)?;
            let observation = store
                .experiment_status(&root, &id)?
                .ok_or_else(|| Error::Invalid("experiment not found".into()))?;
            output(json_mode, &observation, &human_observation(&observation))
        }
        "list" => {
            require(args.is_empty(), "experiment list takes no arguments")?;
            let list = store.experiment_list(&root)?;
            let human = if list.is_empty() {
                "No experiments in this workspace".to_string()
            } else {
                list.iter()
                    .map(human_observation)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output(json_mode, &list, &human)
        }
        "metrics" | "checkpoints" | "events" => {
            let event_type = match command {
                "metrics" => Some("METRIC"),
                "checkpoints" => Some("CHECKPOINT"),
                _ => None,
            };
            let (id, query) = event_query(args, event_type, command == "metrics")?;
            let events = store.experiment_events(&root, &id, &query)?;
            let human = if events.is_empty() {
                "No matching experiment events".into()
            } else {
                events
                    .iter()
                    .map(human_event)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output(json_mode, &events, &human)
        }
        "boundaries" => {
            require(args.len() == 1, "experiment boundaries requires exactly one ID")?;
            let id = ExperimentId::new(args[0]).map_err(Error::Invalid)?;
            let boundaries = store.experiment_boundaries(&root, &id)?;
            let human = if boundaries.decision_boundaries.is_empty() {
                "No decision boundaries declared for this experiment".into()
            } else {
                let mut lines: Vec<String> = boundaries
                    .decision_boundaries
                    .iter()
                    .map(|b| format!("{}  {:?}  {:?}", b.boundary_id, b.condition, b.action))
                    .collect();
                lines.push(format!(
                    "boundaries_hash={}  max_planner_wakeups={}",
                    boundaries.boundaries_hash, boundaries.max_planner_wakeups
                ));
                lines.join("\n")
            };
            output(json_mode, &boundaries, &human)
        }
        "decisions" => {
            require(args.len() == 1, "experiment decisions requires exactly one ID")?;
            let id = ExperimentId::new(args[0]).map_err(Error::Invalid)?;
            let decisions = store.experiment_decisions(&root, &id)?;
            let human = if decisions.is_empty() {
                "No decisions fired for this experiment".into()
            } else {
                decisions
                    .iter()
                    .map(|d| {
                        format!(
                            "{}  attempt={}  boundary={}  {}={} vs threshold  event_sequence={}  decided_at_ms={}",
                            d.decision_id,
                            d.attempt,
                            d.boundary.boundary_id,
                            d.metric_name,
                            d.observed_value,
                            d.triggering_event_sequence,
                            d.decided_at_ms
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output(json_mode, &decisions, &human)
        }
        "wakeups" => {
            require(args.len() == 1, "experiment wakeups requires exactly one ID")?;
            let id = ExperimentId::new(args[0]).map_err(Error::Invalid)?;
            let wakeups = store.experiment_wakeups(&root, &id)?;
            let human = if wakeups.is_empty() {
                "No planner wakeups for this experiment".into()
            } else {
                wakeups
                    .iter()
                    .map(|w| {
                        format!(
                            "{}  decision={}  planning_request={}  status={:?}  planner_jobs={}",
                            w.wakeup.wakeup_id,
                            w.wakeup.decision_id,
                            w.wakeup.planning_request_id.as_str(),
                            w.status,
                            w.planner_jobs.len()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            output(json_mode, &wakeups, &human)
        }
        _ => Err(Error::Invalid(
            "expected experiment run|status|list|cancel|restart|metrics|checkpoints|events|boundaries|decisions|wakeups".into(),
        )),
    }
}
fn output(json_mode: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json_mode {
        println!("{}", crate::local::terminal::json(value)?);
    } else {
        println!("{}", crate::local::terminal::human(human));
    }
    Ok(())
}
