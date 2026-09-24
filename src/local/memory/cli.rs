use super::*;
use std::{collections::BTreeMap, env};

pub(crate) fn run(store: &mut Store, command: &str, args: &[&str], json: bool) -> Result<()> {
    let root = env::current_dir()?;
    let info = graph::checked_workspace(store, &root)?;
    let (value, args) = if [
        "show",
        "links",
        "derive",
        "observe",
        "promote",
        "supersede",
        "reject",
        "search",
    ]
    .contains(&command)
    {
        let (first, tail) = args
            .split_first()
            .ok_or_else(|| Error::Invalid("memory command requires an ID/query".into()))?;
        (Some(*first), tail)
    } else {
        (None, args)
    };
    let mut flags = BTreeMap::<&str, Vec<&str>>::new();
    let mut i = 0;
    while i < args.len() {
        let name = args[i];
        require(
            name.starts_with("--"),
            "unexpected positional memory argument",
        )?;
        if [
            "--workspace",
            "--all",
            "--active",
            "--include-stale",
            "--all-workspaces",
            "--recent",
        ]
        .contains(&name)
        {
            flags.entry(name).or_default().push("true");
            i += 1;
        } else {
            require(i + 1 < args.len(), "memory flag requires a value")?;
            flags.entry(name).or_default().push(args[i + 1]);
            i += 2;
        }
    }
    let allowed: &[&str] = match command {
        "add" => &[
            "--trust",
            "--kind",
            "--content",
            "--key",
            "--actor",
            "--workspace",
            "--supersedes",
            "--job",
            "--symbol",
            "--path",
            "--task",
            "--plan",
            "--evidence",
            "--invariant",
            "--commit",
            "--memory",
            "--tag",
        ],
        "derive" | "observe" => &[],
        "show" | "links" => &["--all-workspaces"],
        "promote" | "reject" => &["--actor"],
        "supersede" => &["--with", "--actor"],
        "list" | "search" | "stale" | "policy" => &[
            "--status",
            "--trust",
            "--kind",
            "--all",
            "--active",
            "--include-stale",
            "--all-workspaces",
            "--recent",
            "--limit",
            "--symbol",
            "--path",
            "--task",
            "--plan",
            "--job",
            "--evidence",
            "--invariant",
            "--commit",
            "--memory",
            "--tag",
        ],
        _ => {
            return Err(Error::Invalid(
                "unknown memory command; run agentctl --help".into(),
            ));
        }
    };
    for (name, values) in &flags {
        require(
            allowed.contains(name),
            format!("unknown flag {name} for memory {command}"),
        )?;
        require(
            values.len() == 1
                || [
                    "--symbol",
                    "--path",
                    "--task",
                    "--plan",
                    "--evidence",
                    "--invariant",
                    "--commit",
                    "--memory",
                    "--tag",
                ]
                .contains(name),
            format!("repeated flag {name}"),
        )?;
    }
    let get = |key: &str| flags.get(key).and_then(|v| v.first()).copied();
    let actor = get("--actor").unwrap_or("local-cli");
    let id = || MemoryId::new(value.unwrap_or_default()).map_err(Error::Invalid);
    match command {
        "add" => {
            let trust = enum_arg(
                get("--trust")
                    .ok_or_else(|| Error::Invalid("explicit --trust is required".into()))?,
            )?;
            let kind = enum_arg(get("--kind").unwrap_or("note"))?;
            let content = get("--content")
                .ok_or_else(|| Error::Invalid("--content is required".into()))?
                .to_string();
            let draft = MemoryDraft {
                kind,
                content,
                workspace_id: get("--workspace").map(|_| info.workspace_id.clone()),
                canonical_key: get("--key").map(str::to_string),
                actor: actor.into(),
                author_job_id: get("--job")
                    .map(JobId::new)
                    .transpose()
                    .map_err(Error::Invalid)?,
                links: links(store, &info, &flags)?,
            };
            let old = get("--supersedes")
                .map(MemoryId::new)
                .transpose()
                .map_err(Error::Invalid)?;
            let entry = store.add_memory(&root, draft, trust, old.as_ref())?;
            output(
                json,
                &entry,
                &format!("Created [{}] {}", label(&trust)?, entry.id.as_str()),
            )
        }
        "derive" => {
            let e = store.derive_memory(&root, value.unwrap())?;
            output(json, &e, &format!("Created [DERIVED] {}", e.id.as_str()))
        }
        "observe" => {
            let e = store.observe_evidence(
                &root,
                &EvidenceId::new(value.unwrap()).map_err(Error::Invalid)?,
            )?;
            output(
                json,
                &e,
                &format!("Created [OBSERVED] {} (historical evidence)", e.id.as_str()),
            )
        }
        "promote" => {
            let e = store.promote_memory(&root, &id()?, actor)?;
            output(
                json,
                &e,
                &format!(
                    "Explicitly promoted {} to new [CANONICAL] {}; original retained",
                    value.unwrap(),
                    e.id.as_str()
                ),
            )
        }
        "supersede" => {
            let new = MemoryId::new(
                get("--with")
                    .ok_or_else(|| Error::Invalid("--with replacement ID is required".into()))?,
            )
            .map_err(Error::Invalid)?;
            store.supersede_memory(&root, &id()?, &new, actor)?;
            let view = store.memory_show(&root, &id()?, false)?;
            output(
                json,
                &view,
                &format!(
                    "{} superseded by {}; history retained",
                    value.unwrap(),
                    new.as_str()
                ),
            )
        }
        "reject" => {
            store.reject_memory(&root, &id()?, actor)?;
            let view = store.memory_show(&root, &id()?, false)?;
            output(
                json,
                &view,
                &format!("Rejected {}; history retained", value.unwrap()),
            )
        }
        "show" | "links" => {
            let view = store.memory_show(&root, &id()?, get("--all-workspaces").is_some())?;
            let links: Vec<String> = view
                .entry
                .links
                .iter()
                .map(crate::local::terminal::json_compact)
                .collect::<serde_json::Result<_>>()?;
            if command == "links" {
                let human = if links.is_empty() {
                    "No links".to_string()
                } else {
                    links.join("\n")
                };
                return output(json, &view.entry.links, &human);
            }
            use crate::local::terminal::{Report, name};
            let e = &view.entry;
            let mut report = Report::new(format!("Memory {}", e.id.as_str()));
            report
                .field("Trust", label(&e.provenance.trust_class)?)
                .field("Kind", name(&e.kind))
                .field("Status", name(&view.status))
                .field(
                    "Validity",
                    format!("{} — {}", name(&view.validity), view.validity_detail),
                )
                .field_opt("Key", e.canonical_key.as_deref())
                .field_opt(
                    "Superseded",
                    view.superseded_by.as_ref().map(MemoryId::as_str),
                )
                .field(
                    "Scope",
                    e.workspace_id.as_ref().map_or("repository", |w| w.as_str()),
                )
                .field("Actor", &e.actor)
                .section("Content")
                .text(e.content.clone());
            if !links.is_empty() {
                report.section("Links").text(links.join("\n"));
            }
            output(json, &view, &report.to_string())
        }
        _ => {
            require(
                ["--all", "--active", "--status"]
                    .iter()
                    .filter(|k| get(k).is_some())
                    .count()
                    <= 1,
                "choose one of --all, --active, or --status",
            )?;
            let query = MemoryQuery {
                text: if command == "search" {
                    value.map(str::to_string)
                } else {
                    None
                },
                trust: get("--trust").map(enum_arg).transpose()?,
                kind: get("--kind").map(enum_arg).transpose()?,
                status: if get("--all").is_some() {
                    None
                } else {
                    Some(
                        get("--status")
                            .map(enum_arg)
                            .transpose()?
                            .unwrap_or(MemoryStatus::Active),
                    )
                },
                links: links(store, &info, &flags)?,
                include_stale: get("--include-stale").is_some() || command == "stale",
                only_stale: command == "stale",
                all_workspaces: get("--all-workspaces").is_some(),
                recent: get("--recent").is_some(),
                limit: get("--limit")
                    .unwrap_or("20")
                    .parse()
                    .map_err(|_| Error::Invalid("invalid memory limit".into()))?,
            };
            let result = store.memory_query(&root, &query)?;
            if command == "policy" {
                let policy =
                    serde_json::json!({"policy": result.policy, "truncated": result.truncated});
                let mut human: Vec<String> = result
                    .policy
                    .iter()
                    .map(|p| format!("{}: {}", p.key, p.content))
                    .collect();
                if result.truncated {
                    human.push("Bounded; narrow filters for more.".into());
                }
                return output(json, &policy, &human.join("\n"));
            }
            let mut human = result
                .policy
                .iter()
                .map(|p| format!("[CANONICAL PROJECT_CONFIG] {}: {}", p.key, p.content))
                .collect::<Vec<_>>();
            for v in &result.entries {
                use crate::local::terminal::name;
                human.push(format!(
                    "[{} {} {}] {} {} scope={}\n  {}",
                    label(&v.entry.provenance.trust_class)?,
                    name(&v.status),
                    name(&v.validity),
                    v.entry.id.as_str(),
                    name(&v.entry.kind),
                    v.entry
                        .workspace_id
                        .as_ref()
                        .map_or("repository", |w| w.as_str()),
                    v.entry.content.chars().take(384).collect::<String>()
                ));
            }
            if human.is_empty() {
                human.push("No matching memory".into());
            }
            if result.truncated {
                human.push("Results/candidate checks bounded; narrow filters for more.".into());
            }
            output(json, &result, &human.join("\n"))
        }
    }
}
fn enum_arg<T: serde::de::DeserializeOwned>(value: &str) -> Result<T> {
    parse_label(&value.to_ascii_uppercase().replace('-', "_"))
}
fn links(
    store: &Store,
    info: &RepositoryInfo,
    flags: &BTreeMap<&str, Vec<&str>>,
) -> Result<Vec<MemoryLink>> {
    let mut links = vec![];
    for (flag, values) in flags {
        for value in values {
            let link = match *flag {
                "--symbol" => {
                    let id = if value.starts_with("graph:") {
                        GraphEntityId::new(*value).map_err(Error::Invalid)?
                    } else {
                        let ids:Vec<String>=store.connection.prepare("SELECT entity_id FROM graph_entities WHERE workspace_id=?1 AND (name=?2 OR qualified_name=?2) ORDER BY entity_id LIMIT 2")?
                            .query_map(params![info.workspace_id.as_str(),value],|r|r.get(0))?.collect::<std::result::Result<_,_>>()?;
                        require(
                            ids.len() == 1,
                            "symbol is absent/ambiguous in the recorded workspace graph; supply a graph ID",
                        )?;
                        GraphEntityId::new(ids[0].clone()).map_err(Error::Invalid)?
                    };
                    MemoryLink::Graph { id }
                }
                "--path" => MemoryLink::File {
                    path: (*value).into(),
                },
                "--task" => MemoryLink::Task {
                    id: TaskId::new(*value).map_err(Error::Invalid)?,
                },
                "--plan" => MemoryLink::Plan {
                    id: PlanId::new(*value).map_err(Error::Invalid)?,
                },
                "--job" => MemoryLink::Job {
                    id: JobId::new(*value).map_err(Error::Invalid)?,
                },
                "--evidence" => MemoryLink::Evidence {
                    id: EvidenceId::new(*value).map_err(Error::Invalid)?,
                },
                "--invariant" => MemoryLink::Invariant {
                    key: (*value).into(),
                },
                "--commit" => MemoryLink::Commit {
                    revision: (*value).into(),
                },
                "--memory" => MemoryLink::Memory {
                    id: MemoryId::new(*value).map_err(Error::Invalid)?,
                },
                "--tag" => MemoryLink::Tag {
                    value: (*value).into(),
                },
                _ => continue,
            };
            links.push(link);
        }
    }
    Ok(links)
}
fn output(json: bool, value: &impl serde::Serialize, human: &str) -> Result<()> {
    if json {
        println!("{}", crate::local::terminal::json(value)?);
    } else {
        println!("{}", crate::local::terminal::human(human));
    }
    Ok(())
}
