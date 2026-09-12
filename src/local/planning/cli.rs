use super::*;
use std::{env, fs::OpenOptions, io::Read};

pub(crate) fn read(path: &str, limit: u64) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    require(
        file.metadata()?.is_file() && file.metadata()?.len() <= limit,
        "planning input must be a bounded regular file",
    )?;
    let mut text = String::new();
    file.take(limit + 1).read_to_string(&mut text)?;
    require(text.len() as u64 <= limit, "planning input exceeds limit")?;
    Ok(text)
}
pub(crate) fn run(store: &mut Store, command: &str, args: &[&str], json: bool) -> Result<()> {
    let root = env::current_dir()?;
    if command == "prepare" {
        require(args.len() % 2 == 0, "prepare flags require values")?;
        let mut flags = BTreeMap::new();
        for pair in args.chunks_exact(2) {
            require(
                [
                    "--objective",
                    "--objective-file",
                    "--request-file",
                    "--query",
                    "--bytes",
                    "--primary",
                    "--neighbors",
                    "--tests",
                    "--files",
                    "--canonical",
                    "--facts",
                    "--notes",
                    "--excerpt-bytes",
                    "--excerpt-lines",
                ]
                .contains(&pair[0]),
                format!("unknown prepare flag {}", pair[0]),
            )?;
            require(
                flags.insert(pair[0], pair[1]).is_none(),
                "repeated prepare flag",
            )?;
        }
        require(
            ["--objective", "--objective-file", "--request-file"]
                .iter()
                .filter(|k| flags.contains_key(**k))
                .count()
                == 1,
            "choose one objective, objective-file or request-file",
        )?;
        let mut intent = if let Some(path) = flags.get("--request-file") {
            serde_json::from_str(&read(path, 16384)?)?
        } else {
            RequestDraft {
                objective: if let Some(s) = flags.get("--objective") {
                    s.to_string()
                } else {
                    read(flags["--objective-file"], 4096)?
                },
                query: None,
                scope: vec![],
                constraints: vec![],
                definition_of_done: vec![],
                verification: None,
                invariant_refs: vec![],
                provenance: PlanningProvenance {
                    actor: "local-cli".into(),
                    source_refs: vec!["explicit-user-objective".into()],
                    provider: None,
                },
            }
        };
        if let Some(q) = flags.get("--query") {
            intent.query = Some(q.to_string());
        }
        let mut limits = PlanningLimits::default();
        for (name, target) in [
            ("--bytes", &mut limits.bytes),
            ("--primary", &mut limits.graph.primary),
            ("--neighbors", &mut limits.graph.neighbors),
            ("--tests", &mut limits.graph.tests),
            ("--files", &mut limits.files),
            ("--canonical", &mut limits.memory.canonical),
            ("--facts", &mut limits.memory.facts),
            ("--notes", &mut limits.memory.notes),
            ("--excerpt-bytes", &mut limits.excerpt_bytes),
            ("--excerpt-lines", &mut limits.excerpt_lines),
        ] {
            if let Some(s) = flags.get(name) {
                *target = s
                    .parse()
                    .map_err(|_| Error::Invalid(format!("invalid numeric {name}")))?;
            }
        }
        let p = store.prepare_plan(&root, intent, limits)?;
        return output(
            json,
            &p,
            &format!(
                "Prepared {}: {} serialized bytes; truncated={}\nUse plan context {} --json for the immutable planner input.",
                p.request.request_id.as_str(),
                p.serialized_bytes,
                p.context.truncated,
                p.request.request_id.as_str()
            ),
        );
    }
    if command == "list" {
        let mut all = false;
        let mut limit = 20;
        let mut rest = args;
        while let Some((first, tail)) = rest.split_first() {
            match *first {
                "--all" if !all => {
                    all = true;
                    rest = tail;
                }
                "--limit" if tail.len() == 1 => {
                    limit = tail[0]
                        .parse()
                        .map_err(|_| Error::Invalid("invalid plan limit".into()))?;
                    rest = &[];
                }
                _ => return Err(Error::Invalid("plan list [--all] [--limit N]".into())),
            }
        }
        let list = store.execution_plans(&root, all, limit)?;
        return output(json, &list, &serde_json::to_string_pretty(&list)?);
    }
    if command == "import" {
        require(args.len() == 1, "plan import requires one JSON file")?;
        let plan: ExecutionPlan = serde_json::from_str(&read(args[0], 262144)?)?;
        let view = store.import_execution_plan(&root, &plan)?;
        return output(
            json,
            &view,
            &format!(
                "Imported and validated {}: {} tasks; {} bytes. Not active.",
                view.plan.packet.plan_id.as_str(),
                view.plan.packet.tasks.len(),
                size(&view.plan)?
            ),
        );
    }
    if command == "context" {
        require(args.len() == 1, "plan context requires a request ID")?;
        let packet = store.planning_context(
            &root,
            &PlanningRequestId::new(args[0]).map_err(Error::Invalid)?,
        )?;
        return output(json, &packet, &serde_json::to_string_pretty(&packet)?);
    }
    let id = PlanId::new(
        *args
            .first()
            .ok_or_else(|| Error::Invalid("plan command requires an ID".into()))?,
    )
    .map_err(Error::Invalid)?;
    match command {
        "supersede" => {
            require(
                args.len() == 3 && args[1] == "--with",
                "plan supersede OLD --with NEW",
            )?;
            store.supersede_execution_plan(
                &root,
                &id,
                &PlanId::new(args[2]).map_err(Error::Invalid)?,
            )?;
        }
        "cancel" => {
            require(
                args.len() == 3 && args[1] == "--reason",
                "plan cancel ID --reason TEXT",
            )?;
            store.cancel_execution_plan(&root, &id, args[2])?;
        }
        "activate" | "validate" | "show" | "export" | "tasks" | "ready" | "blocked" => {
            require(args.len() == 1, "unexpected plan arguments")?;
            if command == "activate" {
                store.activate_execution_plan(&root, &id)?;
            }
            if command == "validate" {
                store.validate_execution_plan(&root, &id)?;
            }
            if ["tasks", "ready", "blocked"].contains(&command) {
                let mut tasks = store.execution_tasks(&root, &id)?;
                if command == "ready" {
                    tasks.retain(|t| t.structurally_ready);
                }
                if command == "blocked" {
                    tasks.retain(|t| !t.structurally_ready && t.state != TaskState::Verified);
                }
                return output(json, &tasks, &serde_json::to_string_pretty(&tasks)?);
            }
        }
        _ => {
            return Err(Error::Invalid(
                "unknown planning command; run agentctl --help".into(),
            ));
        }
    }
    let view = store.execution_plan(&root, &id)?;
    if command == "export" {
        return output(json, &view.plan, &serde_json::to_string_pretty(&view.plan)?);
    }
    output(json, &view, &serde_json::to_string_pretty(&view)?)
}
fn output(json: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}
