use std::{env, error::Error, path::Path, process::ExitCode};

use agentctl::schema;

const HELP: &str = "agentctl — local engineering state and protocol tools

Usage:
  agentctl --version
  agentctl schemas generate [--output <dir>]
  agentctl protocol validate <type> <file>
  agentctl init [--json]
  agentctl doctor [--json]
  agentctl repo init [--json]
  agentctl repo status [--json]
  agentctl repo list [--json]
  agentctl repo index [--status] [--json]
  agentctl code <symbol|search|file|locate|refs|callers|tests> <query> [--limit N] [--json]
  agentctl code <context|impact|neighbors> <query> [--limit N] [--depth N] [--neighbors N] [--tests N] [--json]
    context also accepts --memory-canonical N --memory-facts N --memory-notes N --memory-bytes N
  agentctl state status [--json]
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
        | ["repo", ..]
        | ["state", ..]
        | ["events", ..]
        | ["code", ..]
        | ["memory", ..]) => agentctl::local::cli::run(local)?,
        _ => return Err(format!("invalid arguments\n\n{HELP}").into()),
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(&env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agentctl: {error}");
            ExitCode::FAILURE
        }
    }
}
