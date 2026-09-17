use super::*;
use crate::local::memory::MemoryId;

/// Beyond primary entities, excerpts go to at most this many implementation
/// neighbors and tests; everything else stays graph-level.
const NEIGHBOR_EXCERPTS: usize = 2;
const TEST_EXCERPTS: usize = 2;

/// Prospective-impact bounds for a planner packet. Deliberately far tighter
/// than a standalone report: the planner needs to know that consequences exist
/// outside its neighborhood, not the whole impact graph.
const PLAN_IMPACT_SEEDS: usize = 4;
const PLAN_IMPACT: graph::ImpactLimits = graph::ImpactLimits {
    depth: 2,
    seeds: PLAN_IMPACT_SEEDS,
    items: 8,
    tests: 2,
    boundaries: 4,
    fanout: 32,
};

impl Store {
    /// Prepare once and retain the exact bounded artifact; reading it never rebuilds context.
    pub fn prepare_plan(
        &mut self,
        start: &Path,
        mut intent: RequestDraft,
        limits: PlanningLimits,
    ) -> Result<PlannerPacket> {
        check_intent(&intent)?;
        require(
            (4096..=131072).contains(&limits.bytes)
                && (1..=16).contains(&limits.files)
                && limits.excerpt_bytes <= 4096
                && limits.excerpt_lines <= 80,
            "planning limits: 4–128 KiB total, 1–16 files, 0–4096 excerpt bytes, 0–80 lines",
        )?;
        let info = graph::checked_workspace(self, start)?;
        let policy = ProjectConfig::load(&info.root)?;
        require(
            size(&policy)? <= 16384,
            "planning policy exceeds 16 KiB; use a smaller explicit project policy",
        )?;
        // All project invariants remain critical unless a future policy models applicability.
        intent
            .invariant_refs
            .extend(policy.invariants.keys().cloned());
        intent.invariant_refs.sort();
        intent.invariant_refs.dedup();
        validate_invariants(&self.connection, &info, &policy, &intent.invariant_refs)?;
        let invariants =
            resolve_invariants(&self.connection, &info, &policy, &intent.invariant_refs)?;
        if let Some(v) = &intent.verification {
            checks(&policy, v)?;
        }
        for path in &intent.scope {
            safe_scope(&info.root, &policy, path, false)?;
        }
        let query = if let Some(q) = &intent.query {
            q.clone()
        } else {
            graph::objective_query(&intent.objective)
        };
        let scope = intent.scope.clone();
        let within = |path: &str| scope.is_empty() || scope.iter().any(|s| permits(s, path));
        let mut graph = self
            .graph(start)?
            .context_within(&query, limits.graph, &within)?;
        require(
            graph.freshness.fresh,
            "planning requires a complete fresh graph; inspect repo index --status",
        )?;
        // A planner reasons only against accepted ontology truth, never against
        // an observed candidate that nobody has accepted.
        graph::require_accepted(&self.connection, &info, graph.generation.as_ref())?;
        // File budget: primary implementation first, then the strongest test, then
        // implementation neighbors, then remaining tests.
        let mut files = BTreeSet::new();
        for e in graph
            .primary
            .iter()
            .map(|p| &p.entity)
            .chain(graph.tests.first())
            .chain(graph.neighbors.iter())
            .chain(graph.tests.iter().skip(1))
        {
            if files.len() < limits.files
                && (intent.scope.is_empty()
                    || intent.scope.iter().any(|s| permits(s, &e.provenance.path)))
            {
                files.insert(e.provenance.path.clone());
            }
        }
        graph.truncated |= graph.retain_files(|path| files.contains(path));
        // Prospective impact of editing what the planner is about to reason
        // about, read from the same accepted generation. Discovery is not
        // authorization: nothing here changes the request's scope.
        let seeds: Vec<graph::Entity> = graph
            .primary
            .iter()
            .map(|p| p.entity.clone())
            .take(PLAN_IMPACT_SEEDS)
            .collect();
        let impact = if seeds.is_empty() {
            None
        } else {
            let report = graph::proposed_impact(&self.graph(start)?, &seeds, PLAN_IMPACT)?;
            let scope = intent.scope.clone();
            let covered = scope.clone();
            Some(graph::ImpactOutlook::new(report, scope, &|path| {
                covered.iter().any(|s| permits(s, path))
            }))
        };
        let memory = self.memory_for_code(start, &graph, limits.memory)?;
        let excerpts = excerpts(&info, &graph, limits)?;
        let request_id: String = self.connection.query_row(
            "SELECT 'request:' || lower(hex(randomblob(16)))",
            [],
            |r| r.get(0),
        )?;
        let source = PlanningSource {
            observation: info.source.clone(),
            policy_hash: hash(&policy)?,
            graph_version: graph::INDEX_VERSION.into(),
            graph_generation: graph.generation.clone(),
            support: vec![],
            guarantee: SourceGuarantee::SequentialObservationNotExactDiff,
        };
        let mut packet = PlannerPacket {
            artifact: PlanningArtifactKind::FrozenPlanningInput,
            request: PlanningRequest {
                version: ProtocolVersion::V1,
                request_id: PlanningRequestId::new(request_id).map_err(Error::Invalid)?,
                intent,
                source,
                created_at_ms: now_ms()?,
            },
            context: PlanningContext {
                impact,
                truncated: graph.truncated
                    || memory.truncated
                    || excerpts.iter().any(|e| e.truncated),
                graph,
                memory,
                policy,
                invariants,
                excerpts,
                limits,
            },
            serialized_bytes: 0,
        };
        loop {
            packet.request.source.support = supports(&packet.context);
            measure(&mut packet)?;
            if packet.serialized_bytes <= limits.bytes {
                break;
            }
            packet.context.truncated = true;
            packet.context.graph.truncated = true;
            if !shed(&mut packet.context) {
                return Err(Error::Invalid("required planning intent/policy exceeds byte budget; increase --bytes or narrow intent".into()));
            }
        }
        // Persist only after rechecking selected support and live memory under the write lock.
        let info = graph::checked_workspace(self, start)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_policy = ProjectConfig::load(&info.root)?;
        source_matches(&tx, &info, &packet.request.source, &current_policy)?;
        require(
            resolve_invariants(
                &tx,
                &info,
                &current_policy,
                &packet.request.intent.invariant_refs,
            )? == packet.context.invariants,
            "invariants changed during preparation; retry",
        )?;
        for m in &packet.context.memory.items {
            if m.id.starts_with("memory:") {
                memory::planning_reference(
                    &tx,
                    &info,
                    &MemoryId::new(m.id.clone()).map_err(Error::Invalid)?,
                    limits.memory.notes > 0,
                )?;
            }
        }
        tx.execute(
            "INSERT INTO planning_requests VALUES (?1,?2,?3,?4)",
            params![
                packet.request.request_id.as_str(),
                info.repository_id.as_str(),
                info.workspace_id.as_str(),
                serde_json::to_string(&packet)?
            ],
        )?;
        audit(
            &tx,
            &info,
            None,
            &JournalEntry::PlanningRequestCreated {
                request_id: packet.request.request_id.clone(),
            },
        )?;
        tx.commit()?;
        Ok(packet)
    }

    pub fn planning_context(&self, start: &Path, id: &PlanningRequestId) -> Result<PlannerPacket> {
        let info = graph::checked_workspace(self, start)?;
        request(&self.connection, &info, id)
    }
}

/// Exact, hash-checked excerpts in priority order: implementation primaries,
/// a few implementation neighbors, then a few tests. Whole-file/module ranges and
/// ranges already covered by an earlier excerpt are skipped.
fn excerpts(
    info: &RepositoryInfo,
    graph: &graph::ContextPacket,
    limits: PlanningLimits,
) -> Result<Vec<SourceExcerpt>> {
    let mut excerpts: Vec<SourceExcerpt> = vec![];
    if limits.excerpt_bytes == 0 || limits.excerpt_lines == 0 {
        return Ok(excerpts);
    }
    let excerptable =
        |e: &&graph::Entity| !matches!(e.kind, graph::EntityKind::File | graph::EntityKind::Module);
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    for e in graph
        .primary
        .iter()
        .map(|p| &p.entity)
        .filter(excerptable)
        .chain(
            graph
                .neighbors
                .iter()
                .filter(excerptable)
                .take(NEIGHBOR_EXCERPTS),
        )
        .chain(graph.tests.iter().filter(excerptable).take(TEST_EXCERPTS))
    {
        let start = e.range.start_byte;
        if excerpts.iter().any(|x| {
            x.provenance.path == e.provenance.path && x.start_byte <= start && start < x.end_byte
        }) {
            continue;
        }
        if !sources.contains_key(&e.provenance.path) {
            let (hash, text) = graph::files::read(&info.root, &e.provenance.path)?;
            require(
                hash == e.provenance.content_hash,
                "source changed during excerpt creation; reindex",
            )?;
            sources.insert(e.provenance.path.clone(), text);
        }
        let text = &sources[&e.provenance.path];
        require(
            text.is_char_boundary(start) && e.range.end_byte <= text.len(),
            "invalid indexed source range",
        )?;
        let mut end = e.range.end_byte.min(start + limits.excerpt_bytes);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if let Some((offset, _)) = text[start..end]
            .match_indices('\n')
            .nth(limits.excerpt_lines - 1)
        {
            end = start + offset + 1;
        }
        excerpts.push(SourceExcerpt {
            entity: Some(e.id.clone()),
            provenance: e.provenance.clone(),
            start_byte: start,
            end_byte: end,
            start_line: e.range.start_line,
            text: text[start..end].into(),
            truncated: end < e.range.end_byte,
        });
    }
    Ok(excerpts)
}

/// Removes the least valuable optional record, then everything that referred to
/// it. Order: the impact outlook, unresolved summaries, relations, test
/// excerpts, neighbors (with their excerpts), tests beyond the first, memory,
/// secondary excerpts, the last test, primaries beyond the first, the final
/// excerpt, the final primary.
fn shed(c: &mut PlanningContext) -> bool {
    let g = &mut c.graph;
    let test_excerpt = c.excerpts.iter().rposition(|x| {
        x.entity
            .as_ref()
            .is_some_and(|id| g.tests.iter().any(|t| &t.id == id))
    });
    let dropped =
        if c.impact.take().is_some() || g.unresolved.pop().is_some() || g.relations.pop().is_some()
        {
            true
        } else if let Some(i) = test_excerpt {
            c.excerpts.remove(i);
            true
        } else if g.neighbors.pop().is_some() {
            true
        } else if g.tests.len() > 1 {
            g.tests.pop();
            true
        } else if c.memory.items.pop().is_some() {
            c.memory.truncated = true;
            true
        } else if c.excerpts.len() > 1 {
            c.excerpts.pop();
            true
        } else if g.tests.pop().is_some() {
            true
        } else if g.primary.len() > 1 {
            g.primary.pop();
            true
        } else if c.excerpts.pop().is_some() {
            true
        } else {
            g.primary.pop().is_some()
        };
    if dropped {
        g.prune();
        let ids = g.entity_ids();
        c.excerpts
            .retain(|x| x.entity.as_ref().is_none_or(|id| ids.contains(id)));
    }
    dropped
}

/// Every file whose facts or content the packet carries, bound by provenance.
fn supports(c: &PlanningContext) -> Vec<graph::Provenance> {
    let g = &c.graph;
    let mut map = BTreeMap::new();
    for p in g
        .primary
        .iter()
        .map(|p| &p.entity.provenance)
        .chain(g.tests.iter().map(|e| &e.provenance))
        .chain(g.neighbors.iter().map(|e| &e.provenance))
        .chain(g.relations.iter().map(|e| &e.provenance))
        .chain(c.excerpts.iter().map(|e| &e.provenance))
    {
        map.insert(p.path.clone(), p.clone());
    }
    map.into_values().collect()
}
fn measure(p: &mut PlannerPacket) -> Result<()> {
    loop {
        let bytes = size(p)?;
        if bytes == p.serialized_bytes {
            return Ok(());
        }
        p.serialized_bytes = bytes;
    }
}
