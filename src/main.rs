use std::{env, error::Error, path::Path, process::ExitCode};

use agentctl::schema;

/// Command families in workflow order: `(name, aliases, summary, usage)`.
/// `agentctl --help` lists the summaries; `agentctl <family> --help` prints
/// that family's usage lines.
const FAMILIES: &[(&str, &[&str], &str, &str)] = &[
    (
        "init",
        &[],
        "Create machine-local state (config, database, caches)",
        "  agentctl init [--json]",
    ),
    (
        "doctor",
        &[],
        "Check local state directories, config and database",
        "  agentctl doctor [--json]",
    ),
    (
        "security",
        &[],
        "Report worker isolation on this host and run a live self-test",
        "  agentctl security doctor [--json]",
    ),
    (
        "repo",
        &[],
        "Register this checkout, index its code, attach semantic evidence",
        "  agentctl repo init [--json]
  agentctl repo status [--json]
  agentctl repo list [--json]
  agentctl repo index [--status] [--json]
  agentctl repo enrich [--json]
    enrich attaches what installed semantic providers prove; run it again after re-indexing",
    ),
    (
        "code",
        &[],
        "Query the indexed code graph (symbols, references, callers, impact)",
        "  agentctl code <symbol|search|file|locate|refs|callers|tests> <query> [--limit N] [--json]
  agentctl code <context|impact|neighbors> <query> [--limit N] [--depth N] [--neighbors N] [--tests N] [--json]
    context also accepts --memory-canonical N --memory-facts N --memory-notes N --memory-bytes N",
    ),
    (
        "ontology",
        &[],
        "Inspect, compare, accept or reject ontology generations",
        "  agentctl ontology <status|list> [--limit N] [--json]
  agentctl ontology show <generation-id> [--json]
  agentctl ontology delta [<generation-id> | --from <id> --to <id>] [--change ADDED|REMOVED|MODIFIED] [--path PATH] [--limit N] [--json]
  agentctl ontology footprint [<generation-id> | --from <id> --to <id>] [--plan <plan-id>] [--limit N] [--json]
  agentctl ontology impact [<generation-id> | --from <id> --to <id> | --symbol NAME] [--plan <plan-id>] [--depth N] [--limit N] [--tests N] [--json]
  agentctl ontology accept <generation-id> [--reason TEXT] [--json]
  agentctl ontology reject <generation-id> --reason TEXT [--json]",
    ),
    (
        "plan",
        &[],
        "Prepare planning requests; import, validate and activate plans",
        "  agentctl plan prepare <--objective TEXT|--objective-file PATH|--request-file PATH> [--verify KEY[,KEY]] [--query TEXT] [--bytes N] [--notes N] [--json]
    --verify names the project verification profiles the plan is judged by (default: the single declared profile)
  agentctl plan context <request-id> [--manifest] [--json]
  agentctl plan import <plan.json> [--json]
  agentctl plan <validate|activate|show|export|tasks|ready|blocked> <plan-id> [--json]
  agentctl plan list [--all] [--limit N] [--json]
  agentctl plan supersede <old-id> --with <new-id> [--json]
  agentctl plan cancel <plan-id> --reason TEXT [--json]",
    ),
    (
        "run",
        &[],
        "Execute plans with provider agents; inspect, cancel, restore or replace runs",
        "  agentctl run planner <request-id> [--override role:provider[:model]] [--json]
  agentctl run plan <plan-id> [--override role:provider[:model]] [--json]
  agentctl run plan <plan-id> --dry-run [--json]
  agentctl run resume <plan-id> [--override role:provider[:model]] [--json]
  agentctl run <status|cancel|restore|capabilities|context> <plan-id> [--json]
  agentctl run context decide <plan-id> <decision.json> [--json]
  agentctl run replace <old-plan-id> <validated-replacement-id>",
    ),
    (
        "provider",
        &[],
        "List configured providers; check executables and authentication",
        "  agentctl provider <list|doctor> [--json]",
    ),
    (
        "route",
        &["roles", "role"],
        "Show agent roles and how each resolves to a provider",
        "  agentctl roles [--json]
  agentctl role show <role> [--json]
  agentctl route <role|check> [--override role:provider[:model]] [--json]",
    ),
    (
        "experiment",
        &[],
        "Run long-lived sandboxed jobs with metric boundaries",
        "  agentctl experiment run --program PATH [--arg V]... [--cwd PATH] [--network] [--env NAME]... [--timeout-ms N]
    [--boundary ID:METRIC:OP:VALUE:record[:TAG=VAL,...]]...
    [--boundary ID:METRIC:OP:VALUE:planner:VERIFICATION_REF[:TAG=VAL,...]]...
    [--max-wakeups N] [--json]
  agentctl experiment run --command KEY [--network] [--env NAME]... [--timeout-ms N] [--boundary ...]... [--max-wakeups N] [--json]
    OP is one of < <= > >= ==; a planner boundary needs a verification profile already declared in project policy
    the optional TAG=VAL,... selector matches metric series tags; omitting it matches only an UNTAGGED metric of that name (fail-closed, not a wildcard)
  agentctl experiment <status|reconcile|cancel|restart> <experiment-id> [--json]
  agentctl experiment list [--json]
  agentctl experiment metrics <experiment-id> [--attempt N] [--name NAME] [--limit N] [--json]
  agentctl experiment <checkpoints|events> <experiment-id> [--attempt N] [--limit N] [--json]
  agentctl experiment <boundaries|decisions|wakeups> <experiment-id> [--json]",
    ),
    (
        "memory",
        &[],
        "Record and query provenance-bound engineering knowledge",
        "  agentctl memory add --trust <canonical|agent-note> --kind KIND --content TEXT [--job ID] [--symbol ID] [--invariant KEY] [--key KEY] [--workspace] [--supersedes ID] [--json]
  agentctl memory <show|links|derive|observe|promote|reject> <id-or-symbol> [--json]
  agentctl memory supersede <old-id> --with <new-id> [--json]
  agentctl memory <list|stale|policy> [--trust CLASS] [--kind KIND] [--task ID] [--evidence ID] [--symbol ID] [--all|--status STATUS] [--include-stale] [--all-workspaces] [--recent] [--limit N] [--json]
  agentctl memory search <text> [filters] [--json]",
    ),
    (
        "observe",
        &[],
        "Read the live projection of sessions, agents, tasks and usage",
        "  agentctl observe <snapshot|sessions|agents|tasks|events|experiments|usage> [--json]
  agentctl observe <session|agent|job|task|experiment> <id> [--json]
  agentctl observe usage <provider|task|role> <value> [--json]
  agenttop [--once] [--width N --height N]      interactive view of the same projection",
    ),
    (
        "analytics",
        &[],
        "Summarize job outcomes, token usage, routes and corrections",
        "  agentctl analytics <summary|usage|roles|routes|corrections> [filters] [--json]
  agentctl analytics <session|task|job> <id> [filters] [--json]
    filters: --repository ID --workspace ID --session ID --role ROLE --provider NAME
             --model NAME --task ID --job ID --lifecycle STATE --from-ms N --to-ms N --limit N",
    ),
    (
        "events",
        &[],
        "List the canonical event journal",
        "  agentctl events list [--repo ID] [--task ID] [--job ID] [--limit N] [--json]",
    ),
    (
        "state",
        &[],
        "Show local database schema and record counts",
        "  agentctl state status [--json]",
    ),
    (
        "schemas",
        &[],
        "Write the protocol JSON Schemas (default: schemas/)",
        "  agentctl schemas generate [--output <dir>]",
    ),
    (
        "protocol",
        &[],
        "Validate a protocol document's structure and semantics",
        "  agentctl protocol validate <type> <file>",
    ),
];

fn help() -> String {
    let width = FAMILIES.iter().map(|f| f.0.len()).max().unwrap_or(0);
    let mut text = String::from(
        "agentctl — local engineering control plane for provider agents\n\nUsage: agentctl <command> [args] [--json]\n\nCommands:\n",
    );
    for (name, _, summary, _) in FAMILIES {
        text.push_str(&format!("  {name:<width$}  {summary}\n"));
    }
    text.push_str(
        "\nRun `agentctl <command> --help` for that command's usage.\n\
         --json (always last) prints the canonical machine representation.\n\
         agentctl --version prints the tool and protocol version.",
    );
    text
}

fn family_help(name: &str) -> Option<String> {
    let (family, _, summary, usage) = FAMILIES
        .iter()
        .find(|(family, aliases, ..)| *family == name || aliases.contains(&name))?;
    let mut text = format!("agentctl {family} — {summary}\n\nUsage:\n{usage}");
    if *family == "protocol" {
        text.push_str(&format!(
            "\n\nTypes: {}\nLifecycle proof checks are available through the library.",
            schema::DOCUMENT_TYPES.join(", ")
        ));
    }
    Some(text)
}

fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["--help" | "-h" | "help"] => println!("{}", help()),
        ["help", family] | [family, "--help" | "-h"] | [family, _, "--help" | "-h"] => {
            println!(
                "{}",
                family_help(family).ok_or_else(|| unknown_command(family))?
            )
        }
        ["--version"] => println!("agentctl {} (protocol 1)", env!("CARGO_PKG_VERSION")),
        ["schemas", "generate"] => schema::generate(Path::new("schemas"))?,
        ["schemas", "generate", "--output", dir] => schema::generate(Path::new(dir))?,
        ["protocol", "validate", kind, file] => {
            schema::validate_json(kind, &std::fs::read_to_string(file)?)?;
            println!("valid {kind}: {file}");
        }
        local @ (["init", ..]
        | ["doctor", ..]
        | ["security", ..]
        | ["repo", ..]
        | ["state", ..]
        | ["events", ..]
        | ["code", ..]
        | ["ontology", ..]
        | ["plan", ..]
        | ["run", ..]
        | ["experiment", ..]
        | ["provider", ..]
        | ["analytics", ..]
        | ["roles", ..]
        | ["role", ..]
        | ["route", ..]
        | ["memory", ..]) => agentctl::local::cli::run(local)?,
        local @ ["observe", ..] => agentctl::local::cli::run(local)?,
        [command, ..] => return Err(unknown_command(command).into()),
    }
    Ok(())
}

/// `agentctl: <message>`, with any `agentctl …` command the message
/// recommends repeated as a `Next` block so the recovery step is not buried
/// in the explanation. Canonical codes stay verbatim at the start of the message.
fn render_error(message: &str) -> String {
    let mut next: Vec<&str> = vec![];
    for marker in ["`agentctl ", "run agentctl "] {
        for (at, _) in message.match_indices(marker) {
            let start = at + marker.len() - "agentctl ".len();
            let rest = &message[start..];
            let mut end = rest.find(['`', ';', ',', ')', '\n']).unwrap_or(rest.len());
            for stop in [
                " to ", " or ", " and ", " for ", " before ", " first", " again",
            ] {
                end = end.min(rest.find(stop).unwrap_or(end));
            }
            let command = rest[..end].trim_end_matches(['.', ' ']);
            if !next.contains(&command) {
                next.push(command);
            }
        }
    }
    let mut text = format!("agentctl: {message}\n");
    // A message that already ends with its one recovery command needs no echo.
    let echoes = next.len() == 1 && message.trim_end_matches('.').ends_with(next[0]);
    if !next.is_empty() && !echoes {
        let mut report = agentctl::local::terminal::Report::default();
        report.next(next);
        text.push('\n');
        text.push_str(&report.to_string());
        text.push('\n');
    }
    text
}

fn unknown_command(command: &str) -> String {
    format!(
        "unknown command `{}`; see agentctl --help",
        agentctl::local::terminal::field(command)
    )
}

fn main() -> ExitCode {
    // `agentctl ... | head` closes stdout early, and `println!` panics on the
    // resulting EPIPE. That is a reader that has seen enough, not a failure:
    // end quietly. SIGPIPE itself stays ignored (Rust's default), because the
    // controller writes prompts into provider stdin and a provider that exits
    // early must surface as an error, not kill the controller.
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| info.payload().downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.starts_with("failed printing to stdout") && message.contains("Broken pipe") {
            std::process::exit(0);
        }
        default(info);
    }));
    match run(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Errors routinely quote untrusted text (paths, Git output, IDs).
            eprint!(
                "{}",
                agentctl::local::terminal::human(&render_error(&error.to_string()))
            );
            ExitCode::FAILURE
        }
    }
}
