use std::{env, error::Error, path::Path, process::ExitCode};

use agentctl::schema;

const HELP: &str = "agentctl — Stage 0 protocol tools

Usage:
  agentctl --version
  agentctl schemas generate [--output <dir>]
  agentctl protocol validate <type> <file>

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
