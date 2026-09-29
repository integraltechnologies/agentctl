//! The first process of a provider's lifecycle domain: see
//! `agentctl::runtime::shim`.

fn main() -> std::process::ExitCode {
    agentctl::runtime::shim::main()
}
