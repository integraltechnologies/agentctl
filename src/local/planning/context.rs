use super::*;
use crate::local::memory::MemoryId;

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
            lexical(&intent.objective)
        };
        let mut graph = self.graph(start)?.context(&query, limits.graph)?;
        require(
            graph.freshness.fresh,
            "planning requires a complete fresh graph; inspect repo index --status",
        )?;
        let mut files = BTreeSet::new();
        for e in graph
            .primary
            .iter()
            .map(|p| &p.entity)
            .chain(graph.tests.iter())
            .chain(graph.neighbors.iter())
        {
            if files.len() < limits.files
                && (intent.scope.is_empty()
                    || intent.scope.iter().any(|s| permits(s, &e.provenance.path)))
            {
                files.insert(e.provenance.path.clone());
            }
        }
        let count =
            graph.primary.len() + graph.tests.len() + graph.neighbors.len() + graph.relations.len();
        graph
            .primary
            .retain(|p| files.contains(&p.entity.provenance.path));
        graph.tests.retain(|e| files.contains(&e.provenance.path));
        graph
            .neighbors
            .retain(|e| files.contains(&e.provenance.path));
        graph
            .relations
            .retain(|e| files.contains(&e.provenance.path));
        graph.truncated |= count
            != graph.primary.len()
                + graph.tests.len()
                + graph.neighbors.len()
                + graph.relations.len();
        let memory = self.memory_for_code(start, &graph, limits.memory)?;
        let mut excerpts = vec![];
        let mut seen = BTreeSet::new();
        if limits.excerpt_bytes > 0 && limits.excerpt_lines > 0 {
            for e in graph
                .primary
                .iter()
                .map(|p| &p.entity)
                .chain(graph.tests.iter())
            {
                if !seen.insert(e.provenance.path.clone()) {
                    continue;
                }
                let (hash, text) = graph::files::read(&info.root, &e.provenance.path)?;
                require(
                    hash == e.provenance.content_hash,
                    "source changed during excerpt creation; reindex",
                )?;
                let start = e.range.start_byte;
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
                    provenance: e.provenance.clone(),
                    start_byte: start,
                    end_byte: end,
                    start_line: e.range.start_line,
                    text: text[start..end].into(),
                    truncated: end < e.range.end_byte,
                });
            }
        }
        let request_id: String = self.connection.query_row(
            "SELECT 'request:' || lower(hex(randomblob(16)))",
            [],
            |r| r.get(0),
        )?;
        let source = PlanningSource {
            observation: info.source.clone(),
            policy_hash: hash(&policy)?,
            graph_version: graph::INDEX_VERSION.into(),
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
            packet.request.source.support = supports(&packet.context.graph);
            measure(&mut packet)?;
            if packet.serialized_bytes <= limits.bytes {
                break;
            }
            packet.context.truncated = true;
            let c = &mut packet.context;
            c.graph.truncated = true;
            c.memory.truncated = true;
            if c.excerpts.pop().is_some()
                || c.graph.relations.pop().is_some()
                || c.graph.neighbors.pop().is_some()
                || c.graph.tests.pop().is_some()
                || c.memory.items.pop().is_some()
                || c.graph.primary.pop().is_some()
            {
                continue;
            }
            return Err(Error::Invalid("required planning intent/policy exceeds byte budget; increase --bytes or narrow intent".into()));
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

fn supports(g: &graph::ContextPacket) -> Vec<graph::Provenance> {
    let mut map = BTreeMap::new();
    for p in g
        .primary
        .iter()
        .map(|p| &p.entity.provenance)
        .chain(g.tests.iter().map(|e| &e.provenance))
        .chain(g.neighbors.iter().map(|e| &e.provenance))
        .chain(g.relations.iter().map(|e| &e.provenance))
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
fn lexical(s: &str) -> String {
    let mut out = String::new();
    for token in s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .take(32)
    {
        if out.len() + token.len() + 1 > 512 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(token);
    }
    out
}
