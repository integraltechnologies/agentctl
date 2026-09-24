use super::*;
use crate::local::terminal::{Report, name};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
};

/// Whether the indexed generation's relations carry semantic evidence.
fn semantic_line(stamp: Option<&SemanticStamp>) -> String {
    match stamp {
        Some(stamp) => format!("CURRENT ({})", stamp.providers.join(", ")),
        None => {
            "STRUCTURAL ONLY — relations that need types, traits or macro expansion stay unresolved"
                .into()
        }
    }
}

pub(crate) fn run(store: &mut Store, args: &[&str], json: bool) -> Result<()> {
    let root = env::current_dir()?;
    match args {
        // Explicitly separate from indexing: the deterministic graph is ready
        // in milliseconds and must not wait on a provider that takes seconds.
        ["repo", "enrich"] => {
            let outcomes = store.enrich_semantic(&root)?;
            let mut report = Report::default();
            for outcome in &outcomes {
                match outcome {
                    SemanticOutcome::Current {
                        languages,
                        provider,
                        resolved,
                        occurrences,
                        timing,
                    } => {
                        report
                            .section(format!(
                                "Semantic enrichment ({})",
                                languages
                                    .iter()
                                    .map(|l| format!("{l:?}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                            .field("Status", "CURRENT")
                            .field("Provider", provider)
                            .field("Occurrences", occurrences)
                            .field(
                                "Proven",
                                format!("{resolved} relation(s) syntax could not prove"),
                            )
                            .field(
                                "Timing",
                                format!(
                                    "provider {} ms, parse {} ms, match {} ms, write {} ms",
                                    timing.provider_ms,
                                    timing.parse_ms,
                                    timing.match_ms,
                                    timing.write_ms
                                ),
                            );
                    }
                    SemanticOutcome::Unavailable { languages, reason } => {
                        report
                            .section(format!("Semantic enrichment ({})", languages.iter().map(|l| format!("{l:?}")).collect::<Vec<_>>().join(", ")))
                            .field("Status", "UNAVAILABLE")
                            .field("Reason", reason)
                            .text("Relations syntax cannot prove stay UNKNOWN, which is not proof they are absent.");
                    }
                }
            }
            report
                .section("")
                .text("Re-indexing discards semantic evidence; run agentctl repo enrich again after re-indexing.");
            output(json, &outcomes, &report.to_string())
        }
        ["repo", "index"] => {
            let stats = store.index_repository(&root)?;
            let human = if json {
                String::new()
            } else {
                let mut report = Report::new("Index");
                report
                    .field(
                        "Files",
                        format!(
                            "{} discovered, {} indexed ({} changed), {} reused, {} deleted, {} failed",
                            stats.discovered,
                            stats.indexed,
                            stats.changed,
                            stats.reused,
                            stats.deleted,
                            stats.failed
                        ),
                    )
                    .field(
                        "Graph",
                        format!(
                            "{} entities, {} edges ({} resolved workspace-wide)",
                            stats.entities, stats.edges, stats.resolved
                        ),
                    )
                    .field("Generation", generation_line(stats.generation.as_ref()))
                    .field("Semantic", semantic_line(store.index_status(&root)?.semantic()))
                    .field("Duration", format!("{} ms", stats.duration_ms));
                lifecycle_report(&mut report, &store.ontology_status(&root)?);
                if store.index_status(&root)?.semantic().is_none() {
                    report.next(["agentctl repo enrich    (attach semantic evidence, if a provider is installed)"]);
                }
                report.to_string()
            };
            output(json, &stats, &human)?;
            require(
                stats.failed == 0,
                "index is partial: file failures invalidated old facts; inspect repo index --status",
            )
        }
        ["ontology", command, rest @ ..] => ontology(store, &root, command, rest, json),
        ["repo", "index", "--status"] => {
            let status = store.index_status(&root)?;
            output(json, &status, &{
                let mut report = Report::new("Index");
                report
                    .field("Repository", status.repository_id.as_str())
                    .field("Workspace", status.workspace_id.as_str())
                    .field(
                        "Status",
                        if status.fresh {
                            "FRESH (hash-checked)"
                        } else {
                            "STALE (missing, stale or partial)"
                        },
                    )
                    .field(
                        "Files",
                        format!(
                            "{} indexed, {} stale, {} failed",
                            status.indexed_files,
                            status.stale_files.len(),
                            status.failed_files.len()
                        ),
                    )
                    .field(
                        "Graph",
                        format!("{} entities, {} edges", status.entities, status.edges),
                    )
                    .field(
                        "Last index",
                        status.index.as_ref().map_or("never".into(), |m| {
                            format!("{} ms ({})", m.indexed_at_ms, m.version)
                        }),
                    )
                    .field("Generation", generation_line(status.generation()))
                    .field("Semantic", semantic_line(status.semantic()));
                report.section("Backends");
                for (backend, count) in &status.backends {
                    report.field(backend, format!("{count} file(s)"));
                }
                if !status.fresh {
                    report.next(["agentctl repo index"]);
                }
                report.to_string()
            })
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
                    let mut lines: Vec<String> = result
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
                    // Without this an empty answer reads as "nothing calls it".
                    if let Some(unresolved) = &result.unresolved {
                        lines.push(format!(
                            "{} unresolved site(s) name this symbol and are not proven relations: {}",
                            unresolved.sites,
                            unresolved.paths.join(", ")
                        ));
                    }
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
                        .map(|r| format!("primary   {}", entity_line(&r.entity)))
                        .collect();
                    lines.extend(
                        result
                            .neighbors
                            .iter()
                            .map(|e| format!("neighbor  {}", entity_line(e))),
                    );
                    lines.extend(
                        result
                            .tests
                            .iter()
                            .map(|e| format!("test      {}", entity_line(e))),
                    );
                    lines.push(format!(
                        "\n{} relation(s){}\n{}",
                        result.relations.len(),
                        if result.truncated {
                            "; truncated to limits"
                        } else {
                            ""
                        },
                        result.meaning
                    ));
                    if *command == "context" {
                        let memory = store.memory_for_code(&root, &result, memory_limits)?;
                        for m in &memory.items {
                            lines.push(format!(
                                "memory    [{} {}] {}: {}",
                                name(&m.trust),
                                name(&m.validity),
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
        "footprint" => &["from", "to", "plan", "limit"],
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
            let mut report = Report::default();
            lifecycle_report(&mut report, &status);
            output(json, &status, &report.to_string())
        }
        "list" => {
            require(positional.is_empty(), "ontology list takes no arguments")?;
            let records = store.ontology_generations(root, limit.unwrap_or(20).min(1000))?;
            let mut report = Report::default();
            if records.is_empty() {
                report.text("No ontology generations; run agentctl repo index");
            }
            for record in &records {
                record_report(&mut report, "Generation", record);
            }
            output(json, &records, &report.to_string())
        }
        "show" => {
            let record = store.ontology_generation(root, one("a generation ID")?)?;
            let mut report = Report::default();
            record_report(&mut report, "Generation", &record);
            output(json, &record, &report.to_string())
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
        "footprint" => footprint(store, root, &positional, &flags, limit, json),
        "accept" => {
            let record = store.accept_generation(
                root,
                one("a generation ID")?,
                flags.get("reason").copied(),
            )?;
            let mut report = Report::default();
            record_report(&mut report, "Generation accepted", &record);
            output(json, &record, &report.to_string())
        }
        _ => {
            let reason = flags
                .get("reason")
                .ok_or_else(|| Error::Invalid("ontology reject needs --reason TEXT".into()))?;
            let record = store.reject_generation(root, one("a generation ID")?, reason)?;
            let mut report = Report::default();
            record_report(&mut report, "Generation rejected", &record);
            output(json, &record, &report.to_string())
        }
    }
}

fn record_report(report: &mut Report, title: &str, r: &OntologyGeneration) {
    let origin = match &r.origin {
        GenerationOrigin::External => "external (repo index)".to_string(),
        GenerationOrigin::Runtime { plan_id, task_id } => format!(
            "runtime {}{}",
            plan_id.as_str(),
            task_id
                .as_ref()
                .map(|t| format!(" / {}", t.as_str()))
                .unwrap_or_default()
        ),
    };
    let decision = r.closure.as_ref().or(r.acceptance.as_ref());
    let delta = match &r.delta {
        DeltaStatus::NoBase => "no base".to_string(),
        DeltaStatus::Unavailable { reason } => format!("unavailable: {reason}"),
        DeltaStatus::Recorded { summary: s, .. } => format!(
            "vs {}\nentities +{} -{} ~{}; relations +{} -{}; {} files ({} semantic){}",
            r.base.as_deref().unwrap_or("-"),
            s.entities_added,
            s.entities_removed,
            s.entities_modified,
            s.relations_added,
            s.relations_removed,
            s.files,
            s.semantic_files,
            if s.unproven_identity > 0 {
                format!("; {} with unproven identity", s.unproven_identity)
            } else {
                String::new()
            }
        ),
    };
    report
        .section(title)
        .field("ID", &r.generation_id)
        .field("Status", name(&r.state))
        .field_opt("Decision", decision.map(|d| name(&d.reason)))
        .field("Sequence", r.generation.sequence)
        .field("Origin", origin)
        .field(
            "Content",
            format!(
                "{} files, {} entities, {} relations",
                r.files, r.entities, r.relations
            ),
        )
        .field("Delta", delta);
}

/// The accepted/indexed generation relationship, as its own section.
fn lifecycle_report(report: &mut Report, s: &OntologyStatus) {
    report.section("Ontology").field(
        "Accepted",
        s.accepted.as_ref().map_or("none".into(), |a| {
            format!("{} (sequence {})", a.generation_id, a.generation.sequence)
        }),
    );
    if s.live_accepted {
        report.text("Indexed generation is the accepted generation.");
        return;
    }
    let Some(observed) = &s.observed else {
        report
            .field("Indexed", "no lifecycle record")
            .next(["agentctl repo index"]);
        return;
    };
    report.field(
        "Indexed",
        format!(
            "{} — {}, NOT accepted",
            observed.generation_id,
            name(&observed.state)
        ),
    );
    if observed.state == GenerationState::Candidate {
        report.next([
            format!(
                "Inspect: agentctl ontology delta {}",
                observed.generation_id
            ),
            format!(
                "Accept:  agentctl ontology accept {}",
                observed.generation_id
            ),
        ]);
    }
}

fn delta_lines(full: &SemanticDelta, selected: &SemanticDelta) -> String {
    let s = &full.summary;
    let mut lines = vec![format!(
        "Delta {} (sequence {}) -> {} (sequence {})\n  entities +{} -{} ~{}; relations +{} -{}; {} files ({} semantic); {} unproven identity\n",
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
            "file      {:<9}  {}  ({} entity, {} relation changes)",
            name(&f.content),
            f.path,
            f.entities,
            f.relations
        ));
    }
    for e in &selected.entities {
        let fields = if e.fields.is_empty() {
            String::new()
        } else {
            format!(
                " {}",
                e.fields.iter().map(name).collect::<Vec<_>>().join(",")
            )
        };
        lines.push(format!(
            "entity    {:<9}{}  {:?} {}  {}{}",
            name(&e.change),
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
            "relation  {:<9}  {:?} {} -> {}  ({} -> {})",
            name(&r.change),
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
        "{}  {}:{}  {:?}\n          {}",
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
    if !status.fresh {
        lines.insert(
            0,
            "Index is PARTIAL: failed files are excluded (agentctl repo index --status)\n".into(),
        );
    }
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

/// `agentctl ontology footprint` — bounded structural facts derived from a
/// semantic delta. Like impact, it is read-only and advisory.
fn footprint(
    store: &mut Store,
    root: &Path,
    positional: &[&str],
    flags: &BTreeMap<&str, &str>,
    limit: Option<usize>,
    json: bool,
) -> Result<()> {
    let request = match (flags.get("from"), flags.get("to"), positional) {
        (Some(from), Some(to), []) => FootprintRequest::Diff {
            from: (*from).into(),
            to: (*to).into(),
        },
        (None, None, [id]) => FootprintRequest::Generation((*id).into()),
        (None, None, []) => FootprintRequest::Generation(
            store
                .ontology_status(root)?
                .candidate
                .ok_or_else(|| {
                    Error::Invalid("no open candidate; name a generation or use --from/--to".into())
                })?
                .generation_id,
        ),
        _ => {
            return Err(Error::Invalid(
                "ontology footprint takes one generation ID, or both --from and --to".into(),
            ));
        }
    };
    let mut limits = FootprintLimits::default();
    if let Some(limit) = limit {
        limits.files = limit.min(500);
        limits.entities = limit.min(1000);
        limits.relations = limit.min(1000);
    }
    match flags.get("plan") {
        Some(plan) => {
            let plan = crate::protocol::PlanId::new(*plan).map_err(Error::Invalid)?;
            let outlook = store.plan_footprint(root, &plan, &request, limits)?;
            let text = footprint_lines(&outlook.report, Some(&outlook));
            output(json, &outlook, &text)
        }
        None => {
            let report = store.ontology_footprint(root, &request, limits)?;
            let text = footprint_lines(&report, None);
            output(json, &report, &text)
        }
    }
}

fn footprint_lines(report: &StructuralFootprint, outlook: Option<&FootprintOutlook>) -> String {
    let s = &report.summary;
    let mut lines = vec![format!(
        "{} (sequence {}) -> {} (sequence {})\nfiles +{} -{} ~{} (production +{} -{}, tests +{} -{}); production declarations +{} -{} ~{}; test declarations +{} -{} ~{}; public +{} -{} expanded {}; resolved relations +{} -{}",
        report.from.generation_id,
        report.from.generation.sequence,
        report.to.generation_id,
        report.to.generation.sequence,
        s.files_added,
        s.files_removed,
        s.files_modified,
        s.production_files_added,
        s.production_files_removed,
        s.test_files_added,
        s.test_files_removed,
        s.production_entities_added,
        s.production_entities_removed,
        s.production_entities_modified,
        s.test_entities_added,
        s.test_entities_removed,
        s.test_entities_modified,
        s.public_surface_added,
        s.public_surface_removed,
        s.public_surface_expanded,
        s.relations_added,
        s.relations_removed,
    )];
    for file in &report.files {
        lines.push(format!(
            "file {:?} {:?} {}  ({} entity, {} relation changes; {:?})",
            file.change,
            file.role,
            file.path,
            file.entity_changes,
            file.relation_changes,
            file.role_basis,
        ));
    }
    for entity in report.entities.iter().filter(|e| {
        e.kind != EntityKind::File
            && !(e.kind == EntityKind::Module
                && e.qualified_name
                    == e.path
                        .rsplit_once('.')
                        .map_or(e.path.as_str(), |(stem, _)| stem)
                        .replace('/', "::"))
    }) {
        lines.push(format!(
            "entity {:?} {:?} {:?} {}  {}  surface {:?}->{:?}{}",
            entity.change,
            entity.role,
            entity.kind,
            entity.qualified_name,
            entity.path,
            entity.before_surface,
            entity.after_surface,
            if entity.identity == IdentityBasis::DuplicateOrdinal {
                " [unproven identity]"
            } else {
                ""
            }
        ));
    }
    for relation in &report.relations {
        lines.push(format!(
            "relation {:?} {:?} {} -> {}  ({} -> {})",
            relation.change,
            relation.kind,
            relation.source.as_str(),
            relation.target.as_str(),
            relation.source_path,
            relation.target_path,
        ));
    }
    for signal in &report.signals {
        lines.push(format!(
            "review {:?}: {} ({} evidence, {} omitted)",
            signal.kind,
            signal.meaning,
            signal.evidence.len(),
            signal.evidence_omitted,
        ));
    }
    if let Some(outlook) = outlook {
        lines.push(format!(
            "plan {} {:?}: {}/{} tasks verified; integration proof {}; outside write scope: {} ({} omitted)\nAuthority: {:?}",
            outlook.verification.plan_id.as_str(),
            outlook.verification.state,
            outlook.verification.verified_tasks,
            outlook.verification.total_tasks,
            outlook
                .verification
                .integration_verification
                .as_ref()
                .map_or("pending", |id| id.as_str()),
            if outlook.outside_scope.is_empty() {
                "none".into()
            } else {
                outlook.outside_scope.join(", ")
            },
            outlook.outside_scope_omitted,
            outlook.authority,
        ));
    }
    lines.push(format!(
        "reported/omitted: files {}/{}, entities {}/{}, relations {}/{}, signals {}/{}\n{}",
        s.files_reported,
        s.files_omitted,
        s.entities_reported,
        s.entities_omitted,
        s.relations_reported,
        s.relations_omitted,
        s.signals_reported,
        s.signals_omitted,
        report.meaning,
    ));
    lines.join("\n")
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
