//! Semantic enrichment: proving relations that syntax alone cannot.
//!
//! The fast resolver binds what lexical scope, module structure and imports
//! prove. Everything past that — receiver types, trait and virtual dispatch,
//! overloads, generics, re-exports — needs a real language front end, and
//! agentctl does not write one. Instead a language's own tooling produces an
//! index of *occurrences* (this span is that symbol) and agentctl attaches
//! those facts to observations it already made.
//!
//! The boundary is deliberately two facts wide, in [`Occurrence`]:
//!
//! ```text
//! occurrence: (file, line, column, name) -> symbol
//! definition: symbol                     -> (file, line, name)
//! ```
//!
//! A symbol is an opaque identity; only equality is used. That is the whole
//! contract, and it is language-neutral: it is what SCIP carries, what LSIF
//! carries, and what a Clang, Roslyn or JDT indexer could carry. A provider is
//! a program plus a reader of its output ([`PROVIDERS`]); the reader is the
//! only code that knows a wire format, so adding a language is one more row
//! producing the same facts, never a change to matching or to the graph.
//!
//! Freshness is structural rather than a state machine: workspace resolutions
//! are rebuilt whenever the graph generation changes, which deletes semantic
//! rows along with fast ones, and a provider's output is only attached if the
//! sources it read are still the indexed ones when it finishes. A semantic row
//! therefore cannot outlive the generation it was derived from — if it is
//! present, it is current.
use super::*;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::{collections::HashMap, path::Path, time::Instant};

/// One `(file, line) -> symbol` fact, normalized from whatever a provider
/// emits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Occurrence {
    pub path: String,
    /// 1-based, matching [`SourceRange`].
    pub line: usize,
    /// 0-based offset from the start of the line, in `unit`s.
    pub column: usize,
    pub unit: ColumnUnit,
    /// Opaque identity: equal symbols denote one declaration.
    pub symbol: String,
    /// The declared name the symbol ends in. Cross-checked against what
    /// syntax observed on both ends; never used to find a target by itself.
    pub name: String,
    pub definition: bool,
}

/// What a column counts. Engines differ (Clang counts bytes; Pyright, Roslyn
/// and JDT count UTF-16 units), so a column is only comparable in its unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ColumnUnit {
    Utf8,
    Utf16,
    Utf32,
    /// Not declared: comparable only where every unit agrees (ASCII).
    Unspecified,
}

/// One language's semantic engine, as agentctl runs it: a program found on
/// PATH, the arguments that make it write an index to the path appended last,
/// and the reader that turns that index into [`Occurrence`]s.
struct Provider {
    languages: &'static [Language],
    program: &'static str,
    index: &'static [&'static str],
    read: fn(&[u8]) -> Result<Vec<Occurrence>>,
    install: &'static str,
}

/// Every provider agentctl knows how to run. Both current rows happen to
/// emit SCIP; nothing past `read` knows that.
const PROVIDERS: &[Provider] = &[
    Provider {
        languages: &[Language::Rust],
        program: "rust-analyzer",
        index: &["scip", ".", "--output"],
        read: parse_scip,
        install: "rust-analyzer",
    },
    Provider {
        languages: &[Language::Python],
        program: "scip-python",
        // A project name and version are required by the tool and appear only
        // inside its opaque symbols.
        index: &[
            "index",
            ".",
            "--quiet",
            "--project-name",
            "workspace",
            "--project-version",
            "0",
            "--output",
        ],
        read: parse_scip,
        install: "scip-python (npm package @sourcegraph/scip-python, Pyright-based)",
    },
];

/// Decoded SCIP index: occurrences in file order. Document-local symbols are
/// dropped — they name locals, which are never graph entities.
pub(super) fn parse_scip(bytes: &[u8]) -> Result<Vec<Occurrence>> {
    let mut out = vec![];
    for (field, value) in fields(bytes)? {
        // Index.documents = 2
        if field != 2 {
            continue;
        }
        let mut path = String::new();
        let mut occurrences = vec![];
        let mut unit = ColumnUnit::Unspecified;
        for (field, value) in fields(value)? {
            match field {
                // Document.relative_path = 1
                1 => path = String::from_utf8_lossy(value).into_owned(),
                // Document.occurrences = 2
                2 => occurrences.push(value),
                // Document.position_encoding = 6
                6 => {
                    unit = match varints(value).first() {
                        Some(1) => ColumnUnit::Utf8,
                        Some(2) => ColumnUnit::Utf16,
                        Some(3) => ColumnUnit::Utf32,
                        _ => ColumnUnit::Unspecified,
                    }
                }
                _ => {}
            }
        }
        let path = path.trim_start_matches("./").to_string();
        if path.is_empty() {
            continue;
        }
        for occurrence in occurrences {
            let mut start = None;
            let mut symbol = String::new();
            let mut roles = 0i64;
            for (field, value) in fields(occurrence)? {
                match field {
                    // Occurrence.range = 1, packed [startLine, startChar, ...]
                    1 => {
                        start = match varints(value)[..] {
                            [line, column, ..] if line >= 0 && column >= 0 => {
                                Some((line as usize + 1, column as usize))
                            }
                            _ => None,
                        }
                    }
                    // Occurrence.symbol = 2
                    2 => symbol = String::from_utf8_lossy(value).into_owned(),
                    // Occurrence.symbol_roles = 3
                    3 => roles = varints(value).first().copied().unwrap_or(0),
                    _ => {}
                }
            }
            if symbol.starts_with("local ") {
                continue;
            }
            if let Some((line, column)) = start
                && let Some(name) = descriptor_leaf(&symbol)
            {
                out.push(Occurrence {
                    path: path.clone(),
                    line,
                    column,
                    unit,
                    name: name.to_string(),
                    symbol,
                    // SymbolRole::Definition = 0x1
                    definition: roles & 1 == 1,
                });
            }
        }
    }
    Ok(out)
}

/// Length-delimited and varint protobuf fields at this level. Only the two wire
/// types SCIP uses for the fields above are decoded; anything else is skipped
/// by length so an unknown field can never desynchronize the reader.
fn fields(mut bytes: &[u8]) -> Result<Vec<(u64, &[u8])>> {
    let mut out = vec![];
    while !bytes.is_empty() {
        let (key, rest) = varint(bytes)?;
        let (field, wire) = (key >> 3, key & 7);
        bytes = match wire {
            // length-delimited
            2 => {
                let (len, rest) = varint(rest)?;
                let len = usize::try_from(len).map_err(|_| malformed())?;
                let (value, rest) = rest.split_at_checked(len).ok_or_else(malformed)?;
                out.push((field, value));
                rest
            }
            // varint: kept as its own slice so callers decode it uniformly
            0 => {
                let (_, after) = varint(rest)?;
                out.push((field, &rest[..rest.len() - after.len()]));
                after
            }
            5 => rest.split_at_checked(4).ok_or_else(malformed)?.1,
            1 => rest.split_at_checked(8).ok_or_else(malformed)?.1,
            _ => return Err(malformed()),
        };
    }
    Ok(out)
}

fn varint(bytes: &[u8]) -> Result<(u64, &[u8])> {
    let mut value = 0u64;
    for (i, byte) in bytes.iter().take(10).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value, &bytes[i + 1..]));
        }
    }
    Err(malformed())
}

fn varints(mut bytes: &[u8]) -> Vec<i64> {
    let mut out = vec![];
    while let Ok((value, rest)) = varint(bytes) {
        out.push(value as i64);
        bytes = rest;
        if bytes.is_empty() {
            break;
        }
    }
    out
}

fn malformed() -> Error {
    Error::Invalid("semantic index is not a readable SCIP document".into())
}

/// The name a SCIP symbol ends in. `util/helper().` describes `helper`, and a
/// disambiguated method such as `impl#[Square][Area]area().` still describes
/// `area` — the bracketed self/trait qualifiers are part of the symbol's
/// identity, not of its name. A backticked descriptor such as `` `pkg.util`/ ``
/// is one name even though it contains a dot.
fn descriptor_leaf(symbol: &str) -> Option<&str> {
    let tail = symbol.rsplit(' ').next()?;
    tail.rsplit(['/', '#', '.', '(', ')', '[', ']', '`', '<', '>', ':', '!'])
        .find(|segment| !segment.is_empty())
}

/// An observed relation site the fast pass left unresolved.
struct Site {
    edge: String,
    source: String,
    kind: RelationKind,
    kind_json: String,
    /// The name syntax observed there: `s.area` observes `area`.
    name: String,
    line: usize,
    start_byte: usize,
    end_byte: usize,
}

/// A site's `[start, end)` columns on its first line, in `unit`, or `None`
/// when `unit` is undeclared and the line is not ASCII up to the site's end.
fn columns(source: &str, site: &Site, unit: ColumnUnit) -> Option<(usize, usize)> {
    let start = source.get(..site.start_byte)?;
    let line_start = start.rfind('\n').map_or(0, |i| i + 1);
    let line_end = source[line_start..]
        .find('\n')
        .map_or(source.len(), |i| line_start + i);
    let end = site.end_byte.min(line_end);
    let count = |text: &str| match unit {
        ColumnUnit::Utf8 => Some(text.len()),
        ColumnUnit::Utf16 => Some(text.encode_utf16().count()),
        ColumnUnit::Utf32 => Some(text.chars().count()),
        ColumnUnit::Unspecified => text.is_ascii().then_some(text.len()),
    };
    Some((
        count(source.get(line_start..site.start_byte)?)?,
        count(source.get(line_start..end)?)?,
    ))
}

/// A declaration's extent, for placing a definition inside it.
#[derive(Clone)]
struct Declaration {
    id: String,
    kind: EntityKind,
    start: usize,
    end: usize,
}

/// A relation semantic evidence proved, ready to record.
pub(super) struct Proven {
    edge: String,
    source: String,
    target: String,
    kind_json: String,
}

fn language_of(value: Option<String>) -> Option<Language> {
    value.and_then(|l| serde_json::from_str(&format!("\"{l}\"")).ok())
}

/// Matches occurrences against the unresolved relations of files in
/// `languages`, entirely in memory: the graph is read once, up front.
///
/// Both ends must line up independently. At the site, the occurrence that
/// names the target is the rightmost one starting inside the site's own span
/// (`s.area`, `module::f`, `Enum::Variant` are all named by their last part),
/// and it must carry the name syntax observed there. At the definition, the
/// symbol must have exactly one definition, falling inside exactly one
/// innermost declaration of that name. Anything else abstains, so a
/// mis-aligned index adds nothing rather than something wrong.
///
/// Sites are placed by reading their files, which must still hash to what
/// was indexed; a file that does not contributes nothing.
pub(super) fn matches(
    connection: &Connection,
    workspace: &str,
    root: &Path,
    languages: &[Language],
    occurrences: &[Occurrence],
) -> Result<Vec<Proven>> {
    let ours = |language: Option<Language>| language.is_some_and(|l| languages.contains(&l));
    let mut sites: HashMap<String, Vec<Site>> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT e.edge_id,e.source_id,e.kind,json_extract(e.record_json,'$.target_name'),e.path, \
                json_extract(e.record_json,'$.range.start_line'),json_extract(e.record_json,'$.provenance.language'), \
                json_extract(e.record_json,'$.range.start_byte'),json_extract(e.record_json,'$.range.end_byte') \
         FROM graph_edges e LEFT JOIN graph_resolutions r \
           ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id \
         WHERE e.workspace_id=?1 AND e.target_id IS NULL AND r.edge_id IS NULL \
           AND e.kind IN ('\"CALLS\"','\"REFERENCES\"','\"IMPLEMENTS\"','\"IMPORTS\"')",
    )?;
    let mut rows = statement.query([workspace])?;
    while let Some(row) = rows.next()? {
        if !ours(language_of(row.get(6)?)) {
            continue;
        }
        let kind_json: String = row.get(2)?;
        let target: String = row.get(3)?;
        let name = target.rsplit(['.', ':']).next().unwrap_or_default();
        let path: String = row.get(4)?;
        sites.entry(path).or_default().push(Site {
            edge: row.get(0)?,
            source: row.get(1)?,
            kind: serde_json::from_str(&kind_json)?,
            kind_json,
            name: name.to_string(),
            line: row.get(5)?,
            start_byte: row.get(7)?,
            end_byte: row.get(8)?,
        });
    }
    // By (file, name) for the definition itself, and by file for its owner.
    let mut declarations: HashMap<(String, String), Vec<Declaration>> = HashMap::new();
    let mut by_file: HashMap<String, Vec<Declaration>> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT entity_id,kind,name,path,json_extract(record_json,'$.range.start_line'), \
                json_extract(record_json,'$.range.end_line'),json_extract(record_json,'$.provenance.language') \
         FROM graph_entities WHERE workspace_id=?1",
    )?;
    let mut rows = statement.query([workspace])?;
    while let Some(row) = rows.next()? {
        if !ours(language_of(row.get(6)?)) {
            continue;
        }
        let declaration = Declaration {
            id: row.get(0)?,
            kind: serde_json::from_str(&row.get::<_, String>(1)?)?,
            start: row.get(4)?,
            end: row.get(5)?,
        };
        if matches!(
            declaration.kind,
            EntityKind::Type | EntityKind::Enum | EntityKind::Trait
        ) {
            by_file
                .entry(row.get(3)?)
                .or_default()
                .push(declaration.clone());
        }
        declarations
            .entry((row.get(3)?, row.get(2)?))
            .or_default()
            .push(declaration);
    }
    let indexed: HashMap<String, Option<String>> = connection
        .prepare("SELECT path,content_hash FROM indexed_files WHERE workspace_id=?1")?
        .query_map([workspace], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    // symbol -> its definitions; (file, line) -> references there.
    let mut definitions: HashMap<&str, Vec<&Occurrence>> = HashMap::new();
    let mut references: HashMap<(&str, usize), Vec<&Occurrence>> = HashMap::new();
    for occurrence in occurrences {
        if occurrence.definition {
            definitions
                .entry(&occurrence.symbol)
                .or_default()
                .push(occurrence);
        } else {
            references
                .entry((&occurrence.path, occurrence.line))
                .or_default()
                .push(occurrence);
        }
    }

    let mut proven = vec![];
    for (path, here) in &sites {
        if !here
            .iter()
            .any(|site| references.contains_key(&(path.as_str(), site.line)))
        {
            continue;
        }
        let Ok((hash, source)) = super::files::read(root, path) else {
            continue;
        };
        if indexed.get(path) != Some(&Some(hash)) {
            continue;
        }
        for site in here {
            let Some(on_line) = references.get(&(path.as_str(), site.line)) else {
                continue;
            };
            // The rightmost occurrence(s) starting inside the site's span.
            let inside: Vec<&Occurrence> = on_line
                .iter()
                .copied()
                .filter(|o| {
                    columns(&source, site, o.unit)
                        .is_some_and(|(start, end)| start <= o.column && o.column < end)
                })
                .collect();
            let Some(last) = inside.iter().map(|o| o.column).max() else {
                continue;
            };
            let symbols: BTreeSet<&str> = inside
                .iter()
                .filter(|o| o.column == last)
                .map(|o| o.symbol.as_str())
                .collect();
            let [symbol] = symbols.into_iter().collect::<Vec<_>>()[..] else {
                continue;
            };
            let Some([definition]) = definitions.get(symbol).map(Vec::as_slice) else {
                continue;
            };
            if definition.name != site.name {
                continue;
            }
            let named = declarations
                .get(&(definition.path.clone(), definition.name.clone()))
                .and_then(|all| innermost(all, definition.line));
            // A symbol the graph does not model as an entity — an enum
            // variant, an associated type — is a member of the type declared
            // around it, so a *reference* to it references that owner. Only
            // references: calling a variant or importing a member is not a
            // relation to its owner.
            let target = match (named, site.kind) {
                (Some(target), _) => target,
                (None, RelationKind::References) => match by_file
                    .get(&definition.path)
                    .and_then(|all| innermost(all, definition.line))
                {
                    Some(owner) => owner,
                    None => continue,
                },
                (None, _) => continue,
            };
            if !compatible(site.kind, target.kind) || target.id == site.source {
                continue;
            }
            proven.push(Proven {
                edge: site.edge.clone(),
                source: site.source.clone(),
                target: target.id.clone(),
                kind_json: site.kind_json.clone(),
            });
        }
    }
    Ok(proven)
}

/// The single innermost declaration containing `line`, or none when two
/// equally tight ones do.
fn innermost(all: &[Declaration], line: usize) -> Option<&Declaration> {
    let mut containing: Vec<&Declaration> = all
        .iter()
        .filter(|d| d.start <= line && line <= d.end)
        .collect();
    containing.sort_by_key(|d| d.end - d.start);
    match containing[..] {
        [one] => Some(one),
        [a, b, ..] if a.end - a.start < b.end - b.start => Some(a),
        _ => None,
    }
}

/// Records proven relations, attributed to `provider`. Returns how many were
/// new.
pub(super) fn record(
    connection: &Connection,
    workspace: &str,
    proven: &[Proven],
    provider: &str,
) -> Result<usize> {
    let mut insert = connection.prepare(
        "INSERT OR IGNORE INTO graph_resolutions(workspace_id,edge_id,source_id,target_id,kind,rule,provider) \
         VALUES (?1,?2,?3,?4,?5,'SEMANTIC_PROVIDER',?6)",
    )?;
    let mut resolved = 0;
    for p in proven {
        resolved += insert.execute(params![
            workspace,
            p.edge,
            p.source,
            p.target,
            p.kind_json,
            provider
        ])?;
    }
    Ok(resolved)
}

/// What a relation of this kind may legitimately point at. Semantic evidence
/// widens which relations can be proven, never what a relation may mean.
fn compatible(kind: RelationKind, target: EntityKind) -> bool {
    match kind {
        RelationKind::Calls => matches!(
            target,
            EntityKind::Function | EntityKind::Method | EntityKind::Test | EntityKind::Type
        ),
        RelationKind::References => matches!(
            target,
            EntityKind::Type | EntityKind::Enum | EntityKind::Trait | EntityKind::Constant
        ),
        RelationKind::Implements => target == EntityKind::Trait,
        RelationKind::Imports => super::resolve::importable(target),
        _ => false,
    }
}

/// Where an enrichment spent its time, in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SemanticTiming {
    /// Running the provider program itself.
    pub provider_ms: u64,
    /// Reading its output into occurrences.
    pub parse_ms: u64,
    /// Loading the graph and matching occurrences to it.
    pub match_ms: u64,
    /// Recording proven relations.
    pub write_ms: u64,
}

/// What an enrichment attempt produced for one provider's languages. A
/// provider that is missing, fails, or proves nothing all end in a graph that
/// is exactly as complete as it was — never in a graph that claims more, and
/// never in one that claims less than the deterministic pass already proved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SemanticOutcome {
    /// The provider ran and its facts were attached to this generation.
    Current {
        languages: Vec<Language>,
        provider: String,
        /// Relations proven that the deterministic pass could not.
        resolved: usize,
        occurrences: usize,
        timing: SemanticTiming,
    },
    /// No usable provider for these languages. Relations the fast pass could
    /// not prove stay UNKNOWN — not absent.
    Unavailable {
        languages: Vec<Language>,
        reason: String,
    },
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Runs every provider for a language this workspace has, and attaches what
/// each proves to the current generation.
///
/// Providers are optional and never bundled: agentctl uses a compatible one it
/// finds on PATH. Absence, failure, unreadable output, and sources that change
/// while the provider runs are all the same answer — UNKNOWN stays UNKNOWN.
pub(super) fn run(
    connection: &Connection,
    info: &RepositoryInfo,
    workspace: &str,
) -> Result<Vec<SemanticOutcome>> {
    let present = present_languages(connection, workspace)?;
    let mut outcomes = vec![];
    for provider in PROVIDERS {
        let languages: Vec<Language> = provider
            .languages
            .iter()
            .copied()
            .filter(|l| present.contains(l))
            .collect();
        if !languages.is_empty() {
            outcomes.push(run_one(connection, info, workspace, provider, languages)?);
        }
    }
    let uncovered: Vec<Language> = present
        .into_iter()
        .filter(|l| !PROVIDERS.iter().any(|p| p.languages.contains(l)))
        .collect();
    if !uncovered.is_empty() {
        outcomes.push(SemanticOutcome::Unavailable {
            languages: uncovered,
            reason: "agentctl has no semantic provider for these languages".into(),
        });
    }
    Ok(outcomes)
}

fn run_one(
    connection: &Connection,
    info: &RepositoryInfo,
    workspace: &str,
    provider: &Provider,
    languages: Vec<Language>,
) -> Result<SemanticOutcome> {
    let unavailable = |reason: String| SemanticOutcome::Unavailable {
        languages: languages.clone(),
        reason,
    };
    let version = std::process::Command::new(provider.program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output();
    let identity = match version {
        Ok(output) if output.status.success() => {
            let version = super::parser::compact(&String::from_utf8_lossy(&output.stdout), 64);
            if version.starts_with(provider.program) {
                version
            } else {
                super::parser::compact(&format!("{} {version}", provider.program), 64)
            }
        }
        _ => {
            return Ok(unavailable(format!(
                "{} is not installed; install {} to prove relations syntax cannot",
                provider.program, provider.install
            )));
        }
    };
    // The index goes to a private temp file: it is large, and writing it into
    // the workspace would be a source change agentctl did not authorize.
    // Unique per run, even for concurrent enrichments in one process.
    static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let index = std::env::temp_dir().join(format!(
        "agentctl-semantic-{}-{}-{}-{}.scip",
        provider.program,
        std::process::id(),
        now_ms()?,
        RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let started = Instant::now();
    let output = std::process::Command::new(provider.program)
        .args(provider.index)
        .arg(&index)
        .current_dir(&info.root)
        .stdin(std::process::Stdio::null())
        .output();
    let bytes = output.as_ref().ok().and_then(|output| {
        output
            .status
            .success()
            .then(|| std::fs::read(&index).ok())
            .flatten()
    });
    let _ = std::fs::remove_file(&index);
    let provider_ms = ms(started);
    let bytes = match (output, bytes) {
        (Ok(_), Some(bytes)) if !bytes.is_empty() => bytes,
        // A provider that cannot run, or that ran and produced nothing usable,
        // proves nothing. It must not look like proof of absence.
        (Ok(output), _) => {
            return Ok(unavailable(format!(
                "{identity} exited {} without a readable index; relations it would prove remain unknown",
                output.status.code().unwrap_or(-1)
            )));
        }
        (Err(error), _) => return Ok(unavailable(format!("{identity} could not start: {error}"))),
    };
    // The provider read the working tree, not the index. Its facts are only
    // about the indexed generation if nothing changed while it ran.
    if !super::status(connection, info)?.fresh {
        return Ok(unavailable(format!(
            "sources changed while {identity} ran; its facts describe other source and were discarded"
        )));
    }
    let started = Instant::now();
    let occurrences = match (provider.read)(&bytes) {
        Ok(occurrences) => occurrences,
        Err(error) => return Ok(unavailable(format!("{identity}: {error}"))),
    };
    let parse_ms = ms(started);
    let started = Instant::now();
    let proven = matches(connection, workspace, &info.root, &languages, &occurrences)?;
    let match_ms = ms(started);
    let started = Instant::now();
    let resolved = record(connection, workspace, &proven, &identity)?;
    let write_ms = ms(started);
    Ok(SemanticOutcome::Current {
        languages,
        provider: identity,
        resolved,
        occurrences: occurrences.len(),
        timing: SemanticTiming {
            provider_ms,
            parse_ms,
            match_ms,
            write_ms,
        },
    })
}

/// Languages with at least one indexed entity in this workspace.
fn present_languages(connection: &Connection, workspace: &str) -> Result<Vec<Language>> {
    let mut out = vec![];
    for language in connection
        .prepare("SELECT DISTINCT json_extract(record_json,'$.provenance.language') FROM graph_entities WHERE workspace_id=?1")?
        .query_map([workspace], |row| row.get::<_, Option<String>>(0))?
    {
        if let Some(language) = language_of(language?)
            && !out.contains(&language)
        {
            out.push(language);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One occurrence as a test writes it: line, column, symbol, is-definition.
    pub(super) type TestOccurrence<'a> = (usize, usize, &'a str, bool);

    /// Minimal SCIP writer, so the reader is tested against bytes rather than
    /// against itself.
    pub(super) fn scip_bytes(documents: &[(&str, &[TestOccurrence<'_>])]) -> Vec<u8> {
        fn varint(mut value: u64, out: &mut Vec<u8>) {
            while value >= 0x80 {
                out.push((value as u8) | 0x80);
                value >>= 7;
            }
            out.push(value as u8);
        }
        fn delimited(field: u64, payload: &[u8], out: &mut Vec<u8>) {
            varint(field << 3 | 2, out);
            varint(payload.len() as u64, out);
            out.extend_from_slice(payload);
        }
        let mut index = vec![];
        for (path, occurrences) in documents {
            let mut document = vec![];
            delimited(1, path.as_bytes(), &mut document);
            for (line, column, symbol, definition) in *occurrences {
                let mut occurrence = vec![];
                let mut range = vec![];
                for value in [*line as u64 - 1, *column as u64, *column as u64 + 4] {
                    varint(value, &mut range);
                }
                delimited(1, &range, &mut occurrence);
                delimited(2, symbol.as_bytes(), &mut occurrence);
                varint(3 << 3, &mut occurrence);
                varint(u64::from(*definition), &mut occurrence);
                delimited(2, &occurrence, &mut document);
            }
            delimited(2, &document, &mut index);
        }
        index
    }

    #[test]
    fn scip_documents_and_occurrence_roles_round_trip() {
        let bytes = scip_bytes(&[(
            "src/a.rs",
            &[
                (3, 7, "rust-analyzer cargo c 1.0 a/helper().", true),
                (9, 4, "rust-analyzer cargo c 1.0 a/helper().", false),
            ],
        )]);
        let occurrences = parse_scip(&bytes).unwrap();
        assert_eq!(
            occurrences,
            vec![
                Occurrence {
                    path: "src/a.rs".into(),
                    line: 3,
                    column: 7,
                    unit: ColumnUnit::Unspecified,
                    name: "helper".into(),
                    symbol: "rust-analyzer cargo c 1.0 a/helper().".into(),
                    definition: true,
                },
                Occurrence {
                    path: "src/a.rs".into(),
                    line: 9,
                    column: 4,
                    unit: ColumnUnit::Unspecified,
                    name: "helper".into(),
                    symbol: "rust-analyzer cargo c 1.0 a/helper().".into(),
                    definition: false,
                },
            ]
        );
    }

    #[test]
    fn malformed_semantic_output_is_refused_rather_than_guessed() {
        // Truncated, random and empty input all fail closed: a provider that
        // emits nonsense must not become evidence.
        assert!(parse_scip(&[0xff, 0xff, 0xff]).is_err());
        let valid = scip_bytes(&[("src/a.rs", &[(1, 0, "x y z 1 a/f().", true)])]);
        assert!(parse_scip(&valid[..valid.len() - 2]).is_err());
        assert_eq!(parse_scip(&[]).unwrap(), vec![]);
    }

    #[test]
    fn descriptor_leaf_ignores_symbol_disambiguators() {
        for (symbol, leaf) in [
            ("rust-analyzer cargo c 1.0 util/helper().", "helper"),
            (
                "rust-analyzer cargo c 1.0 shapes/impl#[Square][Area]area().",
                "area",
            ),
            ("rust-analyzer cargo c 1.0 shapes/Square#", "Square"),
            ("rust-analyzer cargo c 1.0 m/`Mul<Self>`mul().", "mul"),
        ] {
            assert_eq!(descriptor_leaf(symbol), Some(leaf), "{symbol}");
        }
    }
}

#[cfg(test)]
mod enrichment {
    use super::tests::scip_bytes;
    use super::*;
    use crate::local::repository::RepositoryInfo;
    use crate::local::store::Store;

    /// Matching and recording in one step, for Rust facts.
    fn enrich(
        connection: &Connection,
        workspace: &str,
        root: &std::path::Path,
        occurrences: &[Occurrence],
        provider: &str,
    ) -> Result<usize> {
        let proven = matches(connection, workspace, root, &[Language::Rust], occurrences)?;
        record(connection, workspace, &proven, provider)
    }

    struct Fixture {
        root: std::path::PathBuf,
        database: std::path::PathBuf,
        workspace: String,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.root.parent().expect("temp root"));
        }
    }

    const SHAPES: &str = "pub struct Square { pub e: u32 }\n\
             impl Square { pub fn area(&self) -> u32 { self.e } }\n\
             pub fn measure(s: &Square) -> u32 { s.area() }\n";

    /// A one-file Rust workspace whose method call the fast pass cannot bind.
    fn fixture() -> Fixture {
        fixture_with(SHAPES)
    }

    fn fixture_with(source: &str) -> Fixture {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let base = std::env::temp_dir().join(format!(
            "agentctl-semantic-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let root = base.join("repo");
        std::fs::create_dir_all(root.join("src")).expect("fixture root");
        std::fs::write(root.join("src/shapes.rs"), source).expect("fixture source");
        for args in [
            vec!["init", "--quiet", "--initial-branch=main"],
            vec!["add", "."],
        ] {
            std::process::Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .expect("git");
        }
        let database = base.join("state.sqlite3");
        let mut store = Store::open(&database, 5000).expect("store");
        let info = RepositoryInfo::discover(&root).expect("discover");
        let workspace = info.workspace_id.as_str().to_string();
        store.register_repository(info).expect("register");
        store.index_repository(&root).expect("index");
        Fixture {
            root,
            database,
            workspace,
        }
    }

    fn unresolved(store: &Store, workspace: &str) -> usize {
        store
            .connection
            .query_row(
                "SELECT count(*) FROM graph_edges e LEFT JOIN graph_resolutions r \
                   ON r.workspace_id=e.workspace_id AND r.edge_id=e.edge_id \
                 WHERE e.workspace_id=?1 AND e.target_id IS NULL AND r.edge_id IS NULL \
                   AND json_extract(e.record_json,'$.kind')='CALLS'",
                [workspace],
                |row| row.get(0),
            )
            .expect("count")
    }

    const METHOD: &str = "rust-analyzer cargo c 1.0 shapes/impl#[Square]area().";
    /// Where `area` starts on the fixture's calling line.
    const AREA: usize = "pub fn measure(s: &Square) -> u32 { s.".len();

    #[test]
    fn semantic_evidence_proves_a_relation_syntax_could_not_and_is_idempotent() {
        let f = fixture();
        let store = Store::open(&f.database, 5000).unwrap();
        let before = unresolved(&store, &f.workspace);
        assert!(before >= 1, "fixture must have an unbound method call");

        let occurrences = scip_bytes(&[(
            "src/shapes.rs",
            &[(2, 0, METHOD, true), (3, AREA, METHOD, false)],
        )]);
        let occurrences = parse_scip(&occurrences).unwrap();
        let resolved = enrich(
            &store.connection,
            &f.workspace,
            &f.root,
            &occurrences,
            "test 1.0",
        )
        .unwrap();
        assert_eq!(resolved, 1, "the method call must be proven");
        assert_eq!(unresolved(&store, &f.workspace), before - 1);

        // The target is the method, and the provider is attributable.
        let (target, provider): (String, String) = store
            .connection
            .query_row(
                "SELECT (SELECT qualified_name FROM graph_entities g WHERE g.entity_id=r.target_id), r.provider \
                 FROM graph_resolutions r WHERE r.rule='SEMANTIC_PROVIDER'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(target.ends_with("area"), "{target}");
        assert_eq!(provider, "test 1.0");

        // Running again adds nothing: one relation, one edge, no competing row.
        let again = enrich(
            &store.connection,
            &f.workspace,
            &f.root,
            &occurrences,
            "test 1.0",
        )
        .unwrap();
        assert_eq!(again, 0, "enrichment must be idempotent");
        let rows: usize = store
            .connection
            .query_row(
                "SELECT count(*) FROM graph_resolutions WHERE rule='SEMANTIC_PROVIDER'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "no duplicate competing edge");
    }

    #[test]
    fn a_span_that_matches_no_observation_proves_nothing() {
        let f = fixture();
        let store = Store::open(&f.database, 5000).unwrap();
        let before = unresolved(&store, &f.workspace);
        for occurrences in [
            // Right symbol, wrong line.
            scip_bytes(&[(
                "src/shapes.rs",
                &[(2, 0, METHOD, true), (99, AREA, METHOD, false)],
            )]),
            // Right line, but the name disagrees with the observation.
            scip_bytes(&[(
                "src/shapes.rs",
                &[
                    (2, 0, "rust-analyzer cargo c 1.0 shapes/other().", true),
                    (3, AREA, "rust-analyzer cargo c 1.0 shapes/other().", false),
                ],
            )]),
            // A reference whose symbol has no definition anywhere in the index.
            scip_bytes(&[("src/shapes.rs", &[(3, AREA, METHOD, false)])]),
            // A file the graph does not contain.
            scip_bytes(&[(
                "src/absent.rs",
                &[(2, 0, METHOD, true), (3, AREA, METHOD, false)],
            )]),
        ] {
            let parsed = parse_scip(&occurrences).unwrap();
            assert_eq!(
                enrich(
                    &store.connection,
                    &f.workspace,
                    &f.root,
                    &parsed,
                    "test 1.0"
                )
                .unwrap(),
                0,
                "mismatched semantic evidence must attach to nothing"
            );
        }
        assert_eq!(unresolved(&store, &f.workspace), before);
    }

    #[test]
    fn re_indexing_discards_semantic_evidence_so_it_cannot_outlive_its_generation() {
        let f = fixture();
        let store = Store::open(&f.database, 5000).unwrap();
        let occurrences = parse_scip(&scip_bytes(&[(
            "src/shapes.rs",
            &[(2, 0, METHOD, true), (3, AREA, METHOD, false)],
        )]))
        .unwrap();
        assert_eq!(
            enrich(
                &store.connection,
                &f.workspace,
                &f.root,
                &occurrences,
                "test 1.0"
            )
            .unwrap(),
            1
        );
        drop(store);

        // The source changes, so every resolution is re-derived from the new
        // facts. Semantic rows go with them: a stale provider claim must never
        // survive to describe source it was not computed from.
        std::fs::write(
            f.root.join("src/shapes.rs"),
            "pub struct Square { pub e: u32 }\n\
             // a line that moves everything below it\n\
             impl Square { pub fn area(&self) -> u32 { self.e } }\n\
             pub fn measure(s: &Square) -> u32 { s.area() }\n",
        )
        .unwrap();
        let mut store = Store::open(&f.database, 5000).unwrap();
        store.index_repository(&f.root).unwrap();
        let rows: usize = store
            .connection
            .query_row(
                "SELECT count(*) FROM graph_resolutions WHERE rule='SEMANTIC_PROVIDER'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "semantic evidence must not outlive its generation");
    }

    #[test]
    fn a_workspace_without_the_provider_language_asks_for_no_provider() {
        let f = fixture();
        let store = Store::open(&f.database, 5000).unwrap();
        // A workspace with no entities has no language to ask a provider about.
        assert!(
            present_languages(&store.connection, "workspace-absent")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            present_languages(&store.connection, &f.workspace).unwrap(),
            vec![Language::Rust]
        );
    }

    fn targets(store: &Store) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = store
            .connection
            .prepare(
                "SELECT json_extract(e.record_json,'$.target_name'), t.qualified_name \
                 FROM graph_resolutions r JOIN graph_edges e ON e.edge_id=r.edge_id \
                 JOIN graph_entities t ON t.entity_id=r.target_id WHERE r.rule='SEMANTIC_PROVIDER'",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows.sort();
        rows
    }

    /// Two same-named calls on one line, to different declarations, are told
    /// apart by column; a line-level matcher could only abstain or guess.
    #[test]
    fn same_named_calls_on_one_line_resolve_to_their_own_targets() {
        let line = "pub fn both(s: &Square, c: &Circle) -> u32 { s.area() + c.area() }";
        let f = fixture_with(&format!(
            "pub struct Square;\nimpl Square {{ pub fn area(&self) -> u32 {{ 1 }} }}\n\
             pub struct Circle;\nimpl Circle {{ pub fn area(&self) -> u32 {{ 2 }} }}\n{line}\n"
        ));
        let store = Store::open(&f.database, 5000).unwrap();
        let square = "rust-analyzer cargo c 1.0 shapes/impl#[Square]area().";
        let circle = "rust-analyzer cargo c 1.0 shapes/impl#[Circle]area().";
        let s_col = line.find("area").unwrap();
        let c_col = line.rfind("area").unwrap();
        let occurrences = parse_scip(&scip_bytes(&[(
            "src/shapes.rs",
            &[
                (2, 0, square, true),
                (4, 0, circle, true),
                (5, s_col, square, false),
                (5, c_col, circle, false),
            ],
        )]))
        .unwrap();
        assert_eq!(
            enrich(&store.connection, &f.workspace, &f.root, &occurrences, "t").unwrap(),
            2
        );
        assert_eq!(
            targets(&store),
            vec![
                ("c.area".into(), "src::shapes::impl Circle::area".into()),
                ("s.area".into(), "src::shapes::impl Square::area".into()),
            ]
        );
    }

    /// The real-repository defect this matcher replaced: an external method
    /// and a local free function share a name on one line. The external
    /// symbol has no definition in the index, so its call site stays unknown
    /// instead of being handed to the local function.
    #[test]
    fn an_external_symbol_never_borrows_a_local_declaration_of_its_name() {
        let line = "pub fn show(p: Paragraph) -> Paragraph { p.block(block()) }";
        let f = fixture_with(&format!(
            "pub struct Paragraph;\npub fn block() -> u32 {{ 1 }}\n{line}\n"
        ));
        let store = Store::open(&f.database, 5000).unwrap();
        let local = "rust-analyzer cargo c 1.0 shapes/block().";
        let external = "rust-analyzer cargo ratatui 0.29 widgets/impl#[Paragraph]block().";
        let occurrences = parse_scip(&scip_bytes(&[(
            "src/shapes.rs",
            &[
                (2, 0, local, true),
                (3, line.find("block").unwrap(), external, false),
                (3, line.rfind("block").unwrap(), local, false),
            ],
        )]))
        .unwrap();
        enrich(&store.connection, &f.workspace, &f.root, &occurrences, "t").unwrap();
        assert!(
            targets(&store).iter().all(|(site, _)| site != "p.block"),
            "{:?}",
            targets(&store)
        );
    }

    /// An occurrence on the right line but outside the site's span, or whose
    /// definition's name disagrees with the observed name, proves nothing.
    #[test]
    fn an_occurrence_off_the_site_span_or_misnamed_proves_nothing() {
        let f = fixture();
        let store = Store::open(&f.database, 5000).unwrap();
        for occurrences in [
            // Right symbol and line, column before the site.
            scip_bytes(&[(
                "src/shapes.rs",
                &[(2, 0, METHOD, true), (3, 0, METHOD, false)],
            )]),
            // A symbol named `area` defined as something named otherwise.
            scip_bytes(&[(
                "src/shapes.rs",
                &[
                    (2, 0, "rust-analyzer cargo c 1.0 shapes/Square#", true),
                    (3, AREA, "rust-analyzer cargo c 1.0 shapes/Square#", false),
                ],
            )]),
        ] {
            let parsed = parse_scip(&occurrences).unwrap();
            assert_eq!(
                enrich(&store.connection, &f.workspace, &f.root, &parsed, "t").unwrap(),
                0
            );
        }
    }

    /// A column is only compared in the unit the provider declared; an
    /// undeclared unit is trusted only where every unit agrees.
    #[test]
    fn columns_are_compared_in_the_declared_unit() {
        let source = "let é = f(😀, g());\n";
        let at = source.find("g()").unwrap();
        let site = Site {
            edge: String::new(),
            source: String::new(),
            kind: RelationKind::Calls,
            kind_json: String::new(),
            name: "g".into(),
            line: 1,
            start_byte: at,
            end_byte: at + 1,
        };
        // `é` is 2 bytes, 1 UTF-16 unit; `😀` is 4 bytes, 2 UTF-16 units.
        assert_eq!(at, 17);
        assert_eq!(columns(source, &site, ColumnUnit::Utf8), Some((17, 18)));
        assert_eq!(columns(source, &site, ColumnUnit::Utf16), Some((14, 15)));
        assert_eq!(columns(source, &site, ColumnUnit::Utf32), Some((13, 14)));
        assert_eq!(columns(source, &site, ColumnUnit::Unspecified), None);
    }

    /// The contract is the normalized occurrence, not SCIP: facts built
    /// directly — as a Clang, Roslyn or JDT adapter would build them, here
    /// with UTF-16 columns — prove exactly what the SCIP route proves.
    #[test]
    fn occurrences_need_no_particular_wire_format() {
        let f = fixture();
        let store = Store::open(&f.database, 5000).unwrap();
        let fact = |line, column, definition| Occurrence {
            path: "src/shapes.rs".into(),
            line,
            column,
            unit: ColumnUnit::Utf16,
            symbol: "any opaque identity".into(),
            name: "area".into(),
            definition,
        };
        let occurrences = [fact(2, 0, true), fact(3, AREA, false)];
        assert_eq!(
            enrich(
                &store.connection,
                &f.workspace,
                &f.root,
                &occurrences,
                "other engine"
            )
            .unwrap(),
            1
        );
        assert_eq!(
            targets(&store),
            vec![("s.area".into(), "src::shapes::impl Square::area".into())]
        );
    }
}
