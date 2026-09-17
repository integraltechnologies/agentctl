use super::*;
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
};

pub(crate) fn run(store: &mut Store, args: &[&str], json: bool) -> Result<()> {
    let root = env::current_dir()?;
    match args {
        ["repo", "index"] => {
            let stats = store.index_repository(&root)?;
            output(
                json,
                &stats,
                &format!(
                    "{} discovered; {} indexed ({} existing changed), {} reused, {} deleted, {} failed\n{} entities, {} edges ({} resolved workspace-wide); {} ms\nGeneration: {}",
                    stats.discovered,
                    stats.indexed,
                    stats.changed,
                    stats.reused,
                    stats.deleted,
                    stats.failed,
                    stats.entities,
                    stats.edges,
                    stats.resolved,
                    stats.duration_ms,
                    generation_line(stats.generation.as_ref())
                ),
            )?;
            if !json {
                println!(
                    "{}",
                    crate::local::terminal::human(&lifecycle_line(&store.ontology_status(&root)?))
                );
            }
            require(
                stats.failed == 0,
                "index is partial: file failures invalidated old facts; inspect repo index --status",
            )
        }
        ["ontology", command, rest @ ..] => ontology(store, &root, command, rest, json),
        ["repo", "index", "--status"] => {
            let status = store.index_status(&root)?;
            output(
                json,
                &status,
                &format!(
                    "Repository: {}\nWorkspace: {}\nIndex: {}\n{} indexed files, {} stale, {} failed; {} entities, {} edges\nLast index: {}\nGeneration: {}\nBackends: {}",
                    status.repository_id.as_str(),
                    status.workspace_id.as_str(),
                    if status.fresh {
                        "hash-checked"
                    } else {
                        "missing/stale/partial"
                    },
                    status.indexed_files,
                    status.stale_files.len(),
                    status.failed_files.len(),
                    status.entities,
                    status.edges,
                    status.index.as_ref().map_or("never".into(), |m| format!(
                        "{} ({})",
                        m.indexed_at_ms, m.version
                    )),
                    generation_line(status.generation()),
                    status
                        .backends
                        .iter()
                        .map(|(backend, count)| format!("{backend}: {count} files"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        }
        ["code", command, query, flags @ ..] => {
            let context = ["context", "impact", "neighbors"].contains(command);
            let mut limits = ContextLimits::default();
            let mut memory_limits = crate::local::memory::MemoryLimits::default();
            let mut limit = if context { limits.primary } else { 20 };
            require(flags.len() % 2 == 0, "query flags need numeric values")?;
            let mut seen = BTreeSet::new();
            for flag in flags.as_chunks::<2>().0 {
                require(seen.insert(flag[0]), "repeated query flag")?;
                let number: usize = flag[1]
                    .parse()
                    .map_err(|_| Error::Invalid("query limits must be positive integers".into()))?;
                match flag[0] {
                    "--limit" => limit = number,
                    "--depth" if context => limits.depth = number,
                    "--neighbors" if context => limits.neighbors = number,
                    "--tests" if context => limits.tests = number,
                    "--memory-canonical" if *command == "context" => {
                        memory_limits.canonical = number
                    }
                    "--memory-facts" if *command == "context" => memory_limits.facts = number,
                    "--memory-notes" if *command == "context" => memory_limits.notes = number,
                    "--memory-bytes" if *command == "context" => memory_limits.bytes = number,
                    _ => return Err(Error::Invalid(format!("unknown query flag {}", flag[0]))),
                }
            }
            let graph = store.graph(&root)?;
            match *command {
                "symbol" | "search" | "file" | "tests" => {
                    let result = match *command {
                        "symbol" => graph.symbols(query, SearchMode::Exact, limit)?,
                        "search" => graph.symbols(query, SearchMode::Substring, limit)?,
                        "file" => graph.entities_in_file(query, limit)?,
                        _ => graph.related_tests(query, limit)?,
                    };
                    let lines = result.data.iter().map(entity_line).collect::<Vec<_>>();
                    output(json, &result, &human_results(&result.freshness, lines))
                }
                "locate" => {
                    let result = graph.locate(query, limit)?;
                    let lines = result
                        .data
                        .iter()
                        .map(|r| {
                            format!(
                                "{}  score={}  {}",
                                entity_line(&r.entity),
                                r.score,
                                r.signals.join(", ")
                            )
                        })
                        .collect();
                    output(json, &result, &human_results(&result.freshness, lines))
                }
                "refs" | "callers" => {
                    let kind = if *command == "callers" {
                        RelationKind::Calls
                    } else {
                        RelationKind::References
                    };
                    let result = graph.relations(query, true, Some(kind), limit)?;
                    let lines = result
                        .data
                        .iter()
                        .map(|e| {
                            format!(
                                "{:?} {} -> {}  {}:{}",
                                e.kind,
                                e.source.as_str(),
                                e.target
                                    .as_ref()
                                    .map_or("unresolved", crate::protocol::GraphEntityId::as_str),
                                e.provenance.path,
                                e.range.start_line
                            )
                        })
                        .collect();
                    output(json, &result, &human_results(&result.freshness, lines))
                }
                "context" | "impact" | "neighbors" => {
                    limits.primary = limit;
                    let result = match *command {
                        "impact" => graph.impact(query, limits)?,
                        "neighbors" => graph.neighborhood(query, limits)?,
                        _ => graph.context(query, limits)?,
                    };
                    let mut lines: Vec<_> = result
                        .primary
                        .iter()
                        .map(|r| format!("primary  {}", entity_line(&r.entity)))
                        .collect();
                    lines.extend(
                        result
                            .neighbors
                            .iter()
                            .map(|e| format!("neighbor {}", entity_line(e))),
                    );
                    lines.extend(
                        result
                            .tests
                            .iter()
                            .map(|e| format!("test     {}", entity_line(e))),
                    );
                    lines.push(format!(
                        "{} relations; bounded/truncated={}\n{}",
                        result.relations.len(),
                        result.truncated,
                        result.meaning
                    ));
                    if *command == "context" {
                        let memory = store.memory_for_code(&root, &result, memory_limits)?;
                        for m in &memory.items {
                            lines.push(format!(
                                "memory [{} {:?}] {}: {}",
                                serde_json::to_string(&m.trust)?.trim_matches('"'),
                                m.validity,
                                m.id,
                                m.content
                            ));
                        }
                        if memory.truncated {
                            lines.push(
                                "Memory is bounded/truncated; use memory show/search for details."
                                    .into(),
                            );
                        }
                        let human = human_results(&result.freshness, lines);
                        let combined = crate::local::memory::CodeContextWithMemory {
                            graph: result,
                            memory,
                        };
                        return output(json, &combined, &human);
                    }
                    output(json, &result, &human_results(&result.freshness, lines))
                }
                _ => Err(Error::Invalid(
                    "unknown code command; run agentctl --help".into(),
                )),
            }
        }
        _ => Err(Error::Invalid(
            "invalid graph command; run agentctl --help".into(),
        )),
    }
}

fn ontology(
    store: &mut Store,
    root: &Path,
    command: &str,
    args: &[&str],
    json: bool,
) -> Result<()> {
    let mut positional = vec![];
    let mut flags = BTreeMap::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if let Some(name) = arg.strip_prefix("--") {
            let value = rest
                .next()
                .ok_or_else(|| Error::Invalid(format!("--{name} needs a value")))?;
            require(
                flags.insert(name, *value).is_none(),
                format!("repeated --{name}"),
            )?;
        } else {
            positional.push(*arg);
        }
    }
    let allowed: &[&str] = match command {
        "status" | "show" => &[],
        "list" => &["limit"],
        "delta" => &["from", "to", "change", "path", "limit"],
        "impact" => &["from", "to", "symbol", "plan", "depth", "limit", "tests"],
        "accept" | "reject" => &["reason"],
        _ => {
            return Err(Error::Invalid(
                "unknown ontology command; run agentctl --help".into(),
            ));
        }
    };
    if let Some(flag) = flags.keys().find(|f| !allowed.contains(f)) {
        return Err(Error::Invalid(format!(
            "unknown flag --{flag} for ontology {command}"
        )));
    }
    let limit = flags
        .get("limit")
        .map(|v| {
            v.parse::<usize>()
                .ok()
                .filter(|n| (1..=10_000).contains(n))
                .ok_or_else(|| Error::Invalid("--limit must be 1–10000".into()))
        })
        .transpose()?;
    let one = |what: &str| -> Result<&str> {
        match positional.as_slice() {
            [id] => Ok(id),
            _ => Err(Error::Invalid(format!("ontology {command} needs {what}"))),
        }
    };
    match command {
        "status" => {
            require(positional.is_empty(), "ontology status takes no arguments")?;
            let status = store.ontology_status(root)?;
            output(json, &status, &lifecycle_line(&status))
        }
        "list" => {
            require(positional.is_empty(), "ontology list takes no arguments")?;
            let records = store.ontology_generations(root, limit.unwrap_or(20).min(1000))?;
            let lines: Vec<_> = records.iter().map(record_line).collect();
            output(json, &records, &lines.join("\n"))
        }
        "show" => {
            let record = store.ontology_generation(root, one("a generation ID")?)?;
            output(json, &record, &record_line(&record))
        }
        "delta" => {
            let delta = match (flags.get("from"), flags.get("to"), positional.as_slice()) {
                (Some(from), Some(to), []) => store.ontology_diff(root, from, to)?,
                (None, None, [id]) => store.ontology_delta(root, id)?,
                (None, None, []) => {
                    let candidate = store.ontology_status(root)?.candidate.ok_or_else(|| {
                        Error::Invalid(
                            "no open candidate; name a generation or use --from/--to".into(),
                        )
                    })?;
                    store.ontology_delta(root, &candidate.generation_id)?
                }
                _ => {
                    return Err(Error::Invalid(
                        "ontology delta takes one generation ID, or both --from and --to".into(),
                    ));
                }
            };
            let change = flags
                .get("change")
                .map(|c| {
                    serde_json::from_value::<Change>(serde_json::Value::String((*c).into()))
                        .map_err(|_| {
                            Error::Invalid("--change must be ADDED, REMOVED or MODIFIED".into())
                        })
                })
                .transpose()?;
            let selected = delta.select(change, flags.get("path").copied(), limit.unwrap_or(200));
            output(json, &selected, &delta_lines(&delta, &selected))
        }
        "impact" => impact(store, root, &positional, &flags, limit, json),
        "accept" => {
            let record = store.accept_generation(
                root,
                one("a generation ID")?,
                flags.get("reason").copied(),
            )?;
            output(json, &record, &format!("Accepted {}", record_line(&record)))
        }
        _ => {
            let reason = flags
                .get("reason")
                .ok_or_else(|| Error::Invalid("ontology reject needs --reason TEXT".into()))?;
            let record = store.reject_generation(root, one("a generation ID")?, reason)?;
            output(json, &record, &format!("Rejected {}", record_line(&record)))
        }
    }
}

fn record_line(r: &OntologyGeneration) -> String {
    let origin = match &r.origin {
        GenerationOrigin::External => "external".to_string(),
        GenerationOrigin::Runtime { plan_id, task_id } => format!(
            "runtime {}{}",
            plan_id.as_str(),
            task_id
                .as_ref()
                .map(|t| format!("/{}", t.as_str()))
                .unwrap_or_default()
        ),
    };
    let decision = r
        .closure
        .as_ref()
        .or(r.acceptance.as_ref())
        .map(|d| format!(" [{:?}]", d.reason))
        .unwrap_or_default();
    let delta = match &r.delta {
        DeltaStatus::NoBase => "no base".to_string(),
        DeltaStatus::Unavailable { reason } => format!("delta unavailable: {reason}"),
        DeltaStatus::Recorded { summary: s, .. } => format!(
            "vs {}: entities +{} -{} ~{}, relations +{} -{}, {} files ({} semantic){}",
            r.base.as_deref().unwrap_or("-"),
            s.entities_added,
            s.entities_removed,
            s.entities_modified,
            s.relations_added,
            s.relations_removed,
            s.files,
            s.semantic_files,
            if s.unproven_identity > 0 {
                format!(", {} with unproven identity", s.unproven_identity)
            } else {
                String::new()
            }
        ),
    };
    format!(
        "{}  {:?}{}  sequence {}  {}\n  {} files, {} entities, {} relations; {}",
        r.generation_id,
        r.state,
        decision,
        r.generation.sequence,
        origin,
        r.files,
        r.entities,
        r.relations,
        delta
    )
}

fn lifecycle_line(s: &OntologyStatus) -> String {
    let accepted = s.accepted.as_ref().map_or("none".into(), |a| {
        format!("{} (sequence {})", a.generation_id, a.generation.sequence)
    });
    let mut lines = vec![format!("Accepted ontology: {accepted}")];
    if s.live_accepted {
        lines.push("Indexed generation is the accepted generation.".into());
    } else if let Some(observed) = &s.observed {
        lines.push(format!(
            "Indexed generation is NOT accepted: {}",
            record_line(observed)
        ));
        if observed.state == GenerationState::Candidate {
            lines.push(format!(
                "Inspect: agentctl ontology delta {id}\nAccept:  agentctl ontology accept {id}",
                id = observed.generation_id
            ));
        }
    } else {
        lines.push("Indexed generation has no lifecycle record; run agentctl repo index".into());
    }
    lines.join("\n")
}

fn delta_lines(full: &SemanticDelta, selected: &SemanticDelta) -> String {
    let s = &full.summary;
    let mut lines = vec![format!(
        "{} (sequence {}) -> {} (sequence {})\nentities +{} -{} ~{}; relations +{} -{}; {} files ({} semantic); {} unproven identity",
        full.from.generation_id,
        full.from.generation.sequence,
        full.to.generation_id,
        full.to.generation.sequence,
        s.entities_added,
        s.entities_removed,
        s.entities_modified,
        s.relations_added,
        s.relations_removed,
        s.files,
        s.semantic_files,
        s.unproven_identity
    )];
    for f in &selected.files {
        lines.push(format!(
            "file {:?}  {}  ({} entity, {} relation changes)",
            f.content, f.path, f.entities, f.relations
        ));
    }
    for e in &selected.entities {
        let fields = if e.fields.is_empty() {
            String::new()
        } else {
            format!(
                " {}",
                e.fields
                    .iter()
                    .map(|f| format!("{f:?}").to_uppercase())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        lines.push(format!(
            "entity {:?}{}  {:?} {}  {}{}",
            e.change,
            if e.identity == IdentityBasis::DuplicateOrdinal {
                " (unproven identity)"
            } else {
                ""
            },
            e.kind,
            e.qualified_name,
            e.path,
            fields
        ));
    }
    for r in &selected.relations {
        lines.push(format!(
            "relation {:?}  {:?} {} -> {}  ({} -> {})",
            r.change,
            r.kind,
            r.source.as_str(),
            r.target.as_str(),
            r.source_path,
            r.target_path
        ));
    }
    lines.join("\n")
}

fn generation_line(generation: Option<&GraphGeneration>) -> String {
    generation.map_or(
        "none (index predates generations; run repo index)".into(),
        |g| format!("{} ({})", g.sequence, g.fingerprint),
    )
}

fn entity_line(e: &Entity) -> String {
    format!(
        "{}  {}:{}  {:?}\n  {}",
        e.qualified_name,
        e.provenance.path,
        e.range.start_line,
        e.kind,
        e.id.as_str()
    )
}
fn human_results(status: &IndexStatus, mut lines: Vec<String>) -> String {
    if lines.is_empty() {
        lines.push("No matching indexed facts".into());
    }
    lines.insert(
        0,
        format!(
            "Workspace: {} — {}",
            status.workspace_id.as_str(),
            if status.fresh {
                "hash-checked source observation"
            } else {
                "PARTIAL: failed files excluded (see repo index --status)"
            }
        ),
    );
    lines.join("\n")
}
fn output(json: bool, value: &impl Serialize, human: &str) -> Result<()> {
    if json {
        println!("{}", crate::local::terminal::json(value)?);
    } else {
        println!("{}", crate::local::terminal::human(human));
    }
    Ok(())
}

/// `agentctl ontology impact` — the evidence-backed consequences of an observed
/// or proposed change. It reports; it never changes scope or state.
fn impact(
    store: &mut Store,
    root: &Path,
    positional: &[&str],
    flags: &BTreeMap<&str, &str>,
    limit: Option<usize>,
    json: bool,
) -> Result<()> {
    let number = |name: &str| -> Result<Option<usize>> {
        flags
            .get(name)
            .map(|v| {
                v.parse::<usize>()
                    .map_err(|_| Error::Invalid(format!("--{name} must be a whole number")))
            })
            .transpose()
    };
    let mut limits = ImpactLimits::default();
    if let Some(depth) = number("depth")? {
        limits.depth = depth;
    }
    if let Some(items) = limit {
        limits.items = items;
    }
    if let Some(tests) = number("tests")? {
        limits.tests = tests;
    }
    let request = match (
        flags.get("symbol"),
        flags.get("from"),
        flags.get("to"),
        positional,
    ) {
        (Some(symbol), None, None, []) => ImpactRequest::Symbols(vec![(*symbol).into()]),
        (None, Some(from), Some(to), []) => ImpactRequest::Diff {
            from: (*from).into(),
            to: (*to).into(),
        },
        (None, None, None, [id]) => ImpactRequest::Generation((*id).into()),
        (None, None, None, []) => ImpactRequest::Generation(
            store
                .ontology_status(root)?
                .candidate
                .ok_or_else(|| {
                    Error::Invalid(
                        "no open candidate; name a generation, or use --from/--to or --symbol"
                            .into(),
                    )
                })?
                .generation_id,
        ),
        _ => {
            return Err(Error::Invalid(
                "ontology impact takes one generation ID, or both --from and --to, or --symbol NAME"
                    .into(),
            ));
        }
    };
    match flags.get("plan") {
        Some(plan) => {
            let plan = crate::protocol::PlanId::new(*plan).map_err(Error::Invalid)?;
            let outlook = store.plan_impact(root, &plan, &request, limits)?;
            let text = outlook_lines(&outlook);
            output(json, &outlook, &text)
        }
        None => {
            let report = store.ontology_impact(root, &request, limits)?;
            let text = impact_lines(&report);
            output(json, &report, &text)
        }
    }
}

fn impact_lines(r: &ImpactReport) -> String {
    let mut lines = vec![format!(
        "Generation {} (sequence {})",
        r.generation.fingerprint, r.generation.sequence
    )];
    for seed in &r.seeds {
        lines.push(format!(
            "seed     {} {}  {}{}",
            seed.qualified_name,
            seed.path,
            serde_json::to_string(&seed.origin).unwrap_or_default(),
            if seed.skipped {
                "  [not traversed]"
            } else {
                ""
            }
        ));
    }
    for item in &r.items {
        lines.push(format!(
            "{:?} d{} {} {}\n    {}",
            item.class,
            item.distance,
            item.qualified_name,
            item.path,
            evidence_line(item)
        ));
    }
    for boundary in &r.boundaries {
        lines.push(format!(
            "boundary {} {}  {}",
            boundary.qualified_name,
            boundary.path,
            serde_json::to_string(&boundary.reason).unwrap_or_default()
        ));
    }
    let s = &r.summary;
    lines.push(format!(
        "{} seeds ({} not traversed, {} omitted); {} items in {} files (direct {}, contract {}, verification {}, containment {}, cross-file {}, {} omitted); {} boundaries ({} omitted); max distance {}\n{}",
        s.seeds,
        s.seeds_skipped,
        s.seeds_omitted,
        s.items,
        s.files,
        s.direct,
        s.contract,
        s.verification,
        s.containment,
        s.cross_file,
        s.items_omitted,
        s.boundaries,
        s.boundaries_omitted,
        s.max_distance,
        r.meaning
    ));
    lines.join("\n")
}

/// The chain read from the seed outwards. Each hop names the relation the
/// reached entity has *to* the entity before it, so the arrow points at the
/// source of truth: `seed <-[CALLS]- caller`.
fn evidence_line(item: &ImpactItem) -> String {
    let short = |id: &crate::protocol::GraphEntityId| {
        let text = id.as_str();
        text.strip_prefix("graph:").map_or_else(
            || text.to_string(),
            |h| format!("graph:{}", &h[..12.min(h.len())]),
        )
    };
    let mut chain = vec![
        item.evidence
            .first()
            .map_or(String::new(), |s| short(&s.from)),
    ];
    for step in &item.evidence {
        chain.push(format!(
            "<-[{}]- {}",
            match &step.edge {
                ImpactEdge::Relation { kind, removed, .. } =>
                    format!("{:?}{}", kind, if *removed { " removed" } else { "" }),
                ImpactEdge::Containment => "CONTAINS".into(),
                ImpactEdge::TestAssociation { basis } => format!("TEST:{basis:?}"),
            },
            short(&step.entity)
        ));
    }
    chain.join(" ")
}

fn outlook_lines(o: &ImpactOutlook) -> String {
    format!(
        "{}\nDeclared write scope: {}\nOutside declared scope: {} items in {} files{}\nAuthority: {:?} — impact never widens read or write scope.",
        impact_lines(&o.report),
        if o.scope.is_empty() {
            "none declared".to_string()
        } else {
            o.scope
                .iter()
                .map(|s| s.path().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        },
        o.outside_scope_items,
        o.outside_scope.len(),
        if o.outside_scope.is_empty() {
            String::new()
        } else {
            format!("\n  {}", o.outside_scope.join("\n  "))
        },
        o.authority
    )
}
