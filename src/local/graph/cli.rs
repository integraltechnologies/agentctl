use super::*;
use serde::Serialize;
use std::{collections::BTreeSet, env};

pub(crate) fn run(store: &mut Store, args: &[&str], json: bool) -> Result<()> {
    let root = env::current_dir()?;
    match args {
        ["repo", "index"] => {
            let stats = store.index_repository(&root)?;
            output(
                json,
                &stats,
                &format!(
                    "{} discovered; {} indexed ({} existing changed), {} reused, {} deleted, {} failed\n{} entities, {} edges; {} ms",
                    stats.discovered,
                    stats.indexed,
                    stats.changed,
                    stats.reused,
                    stats.deleted,
                    stats.failed,
                    stats.entities,
                    stats.edges,
                    stats.duration_ms
                ),
            )?;
            require(
                stats.failed == 0,
                "index is partial: file failures invalidated old facts; inspect repo index --status",
            )
        }
        ["repo", "index", "--status"] => {
            let status = store.index_status(&root)?;
            output(
                json,
                &status,
                &format!(
                    "Repository: {}\nWorkspace: {}\nIndex: {}\n{} indexed files, {} stale, {} failed; {} entities, {} edges\nLast index: {}\nBackends: {}",
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
            let mut limit = if context { limits.primary } else { 20 };
            require(flags.len() % 2 == 0, "query flags need numeric values")?;
            let mut seen = BTreeSet::new();
            for flag in flags.chunks_exact(2) {
                require(seen.insert(flag[0]), "repeated query flag")?;
                let number: usize = flag[1]
                    .parse()
                    .map_err(|_| Error::Invalid("query limits must be positive integers".into()))?;
                match flag[0] {
                    "--limit" => limit = number,
                    "--depth" if context => limits.depth = number,
                    "--neighbors" if context => limits.neighbors = number,
                    "--tests" if context => limits.tests = number,
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
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{human}");
    }
    Ok(())
}
