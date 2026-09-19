use std::{env, error::Error, path::Path, process::ExitCode};

use agentctl::schema;

const HELP: &str = "agentctl — local engineering state and protocol tools

Usage:
  agentctl --version
  agentctl schemas generate [--output <dir>]
  agentctl protocol validate <type> <file>
  agentctl init [--json]
  agentctl doctor [--json]
  agentctl security doctor [--json]
  agentctl repo init [--json]
  agentctl repo status [--json]
  agentctl repo list [--json]
  agentctl repo index [--status] [--json]
  agentctl ontology <status|list> [--limit N] [--json]
  agentctl ontology show <generation-id> [--json]
  agentctl ontology delta [<generation-id> | --from <id> --to <id>] [--change ADDED|REMOVED|MODIFIED] [--path PATH] [--limit N] [--json]
  agentctl ontology footprint [<generation-id> | --from <id> --to <id>] [--plan <plan-id>] [--limit N] [--json]
  agentctl ontology impact [<generation-id> | --from <id> --to <id> | --symbol NAME] [--plan <plan-id>] [--depth N] [--limit N] [--tests N] [--json]
  agentctl ontology accept <generation-id> [--reason TEXT] [--json]
  agentctl ontology reject <generation-id> --reason TEXT [--json]
  agentctl code <symbol|search|file|locate|refs|callers|tests> <query> [--limit N] [--json]
  agentctl code <context|impact|neighbors> <query> [--limit N] [--depth N] [--neighbors N] [--tests N] [--json]
    context also accepts --memory-canonical N --memory-facts N --memory-notes N --memory-bytes N
  agentctl state status [--json]
  agentctl observe <snapshot|sessions|agents|tasks|events|experiments|usage> [--json]
  agentctl observe <session|agent|job|task|experiment> <id> [--json]
  agentctl observe usage <provider|task|role> <value> [--json]
  agentctl analytics <summary|usage|roles|routes|corrections> [filters] [--json]
  agentctl analytics <session|task|job> <id> [filters] [--json]
    filters: --repository ID --workspace ID --session ID --role ROLE --provider NAME
             --model NAME --task ID --job ID --lifecycle STATE --from-ms N --to-ms N --limit N
  agenttop [--once] [--width N --height N]
  agentctl provider <list|doctor> [--json]
  agentctl roles [--json]
  agentctl role show <role> [--json]
  agentctl route <role|check> [--override role:provider[:model]] [--json]
  agentctl run <planner|plan|resume> <id> [--override role:provider[:model]] [--json]
  agentctl run planner <request-id> [--json]
  agentctl run plan <plan-id> [--dry-run] [--json]
  agentctl run <resume|status|cancel> <plan-id> [--json]
  agentctl run replace <old-plan-id> <validated-replacement-id>
  agentctl run packet-hashes < plan-packet.json
  agentctl experiment run --program PATH [--arg V]... [--cwd PATH] [--network] [--env NAME]... [--timeout-ms N]
    [--boundary ID:METRIC:OP:VALUE:record[:TAG=VAL,...]]...
    [--boundary ID:METRIC:OP:VALUE:planner:VERIFICATION_REF[:TAG=VAL,...]]...
    [--max-wakeups N] [--json]
  agentctl experiment run --command KEY [--network] [--env NAME]... [--timeout-ms N] [--boundary ...]... [--max-wakeups N] [--json]
    OP is one of < <= > >= ==; a planner boundary needs a verification profile already declared in project policy
    the optional TAG=VAL,... selector matches metric series tags; omitting it matches only an UNTAGGED metric of that name (fail-closed, not a wildcard)
  agentctl experiment status <experiment-id> [--json]
  agentctl experiment list [--json]
  agentctl experiment cancel <experiment-id> [--json]
  agentctl experiment restart <experiment-id> [--json]
  agentctl experiment metrics <experiment-id> [--attempt N] [--name NAME] [--limit N] [--json]
  agentctl experiment checkpoints <experiment-id> [--attempt N] [--limit N] [--json]
  agentctl experiment events <experiment-id> [--attempt N] [--limit N] [--json]
  agentctl experiment boundaries <experiment-id> [--json]
  agentctl experiment decisions <experiment-id> [--json]
  agentctl experiment wakeups <experiment-id> [--json]
  agentctl plan prepare <--objective TEXT|--objective-file PATH|--request-file PATH> [--query TEXT] [--bytes N] [--notes N] [--json]
  agentctl plan context <request-id> [--manifest] [--json]
  agentctl plan import <plan.json> [--json]
  agentctl plan <validate|activate|show|export|tasks|ready|blocked> <plan-id> [--json]
  agentctl plan list [--all] [--limit N] [--json]
  agentctl plan supersede <old-id> --with <new-id> [--json]
  agentctl plan cancel <plan-id> --reason TEXT [--json]
  agentctl memory add --trust <canonical|agent-note> --kind KIND --content TEXT [--job ID] [--symbol ID] [--invariant KEY] [--key KEY] [--workspace] [--supersedes ID] [--json]
  agentctl memory <show|links|derive|observe|promote|reject> <id-or-symbol> [--json]
  agentctl memory supersede <old-id> --with <new-id> [--json]
  agentctl memory <list|stale|policy> [--trust CLASS] [--kind KIND] [--task ID] [--evidence ID] [--symbol ID] [--all|--status STATUS] [--include-stale] [--all-workspaces] [--recent] [--limit N] [--json]
  agentctl memory search <text> [filters] [--json]
  agentctl events list [--repo ID] [--task ID] [--job ID] [--limit N] [--json]

Schema output defaults to schemas/. Validation checks structure and document
semantics; lifecycle proof checks are available through the library.";

fn run(args: &[String]) -> Result<(), Box<dyn Error>> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["--help"] | ["-h"] => {
            println!(
                "{HELP}\n\nProtocol types: {}",
                schema::DOCUMENT_TYPES.join(", ")
            );
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
        _ => return Err(format!("invalid arguments\n\n{HELP}").into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Errors routinely quote untrusted text (paths, Git output, IDs).
            eprintln!(
                "agentctl: {}",
                agentctl::local::terminal::human(&error.to_string())
            );
            ExitCode::FAILURE
        }
    }
}
