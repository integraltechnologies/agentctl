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
        "{}  liveness={:?}  events={}  ingestion_errors={}",
        human_run(&o.run),
        o.liveness,
        o.events.event_count,
        o.events.ingestion_errors
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
            let mut runtime = ExperimentRuntime::new(store, paths.clone())?;
            let run = if let Some(key) = project_command {
                runtime.run_project_command(&root, key, network, env_passthrough, timeout_ms)?
            } else {
                runtime.run(
                    &root,
                    ExperimentInput {
                        command: command.expect("exactly one command source was checked"),
                        network,
                        env_passthrough,
                        timeout_ms,
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
            let mut runtime = ExperimentRuntime::new(store, paths.clone())?;
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
        _ => Err(Error::Invalid(
            "expected experiment run|status|list|cancel|restart|metrics|checkpoints|events".into(),
        )),
    }
}
fn output(json_mode: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}
