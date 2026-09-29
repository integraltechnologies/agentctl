//! `agenttop`: a live, read-only monitor of the project's agent work and
//! token usage. See `agentctl::top`.

use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use agentctl::project::Project;
use agentctl::top::{Input, View};

#[derive(Parser)]
#[command(
    name = "agenttop",
    version,
    about = "Live monitor of agentctl work and token usage"
)]
struct Args {
    /// Milliseconds between reads of the project's state.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(100..))]
    interval: u64,
}

fn input(wait: Duration) -> std::io::Result<Option<Input>> {
    if !event::poll(wait)? {
        return Ok(None);
    }
    Ok(Some(match event::read()? {
        Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Input::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Input::Quit,
            KeyCode::Tab => Input::Next,
            KeyCode::BackTab => Input::Prev,
            KeyCode::Char(c @ '1'..='6') => match View::from_digit(c) {
                Some(view) => Input::Select(view),
                None => Input::Redraw,
            },
            _ => Input::Redraw,
        },
        _ => Input::Redraw,
    }))
}

fn run() -> Result<()> {
    let args = Args::parse();
    if !std::io::stdout().is_terminal() {
        bail!(
            "a terminal is required: agenttop draws an interactive screen and cannot write to a pipe or file"
        );
    }
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let Some(project) = Project::discover(&cwd)? else {
        bail!("no agentctl project here (no agentctl.toml in this or any parent directory)");
    };
    let mut terminal = match ratatui::try_init() {
        Ok(terminal) => terminal,
        Err(e) => {
            ratatui::restore();
            return Err(e).context("initializing the terminal");
        }
    };
    let result = agentctl::top::run(
        &mut terminal,
        &project,
        Duration::from_millis(args.interval),
        input,
    );
    ratatui::restore();
    result
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agenttop: {e:#}");
            ExitCode::FAILURE
        }
    }
}
