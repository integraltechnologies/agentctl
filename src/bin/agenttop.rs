fn main() -> std::process::ExitCode {
    match agentctl::local::agenttop::run(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agenttop: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
