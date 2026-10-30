//! kprtfwd: Kubernetes port-forward manager. Without a command it opens the
//! TUI.

use std::process::ExitCode;

use clap::Parser;
use kprtfwd_cli::{Command, DaemonCommand};

#[derive(Parser)]
#[command(name = "kprtfwd", version, about = "Kubernetes port-forward manager")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

fn main() -> ExitCode {
    let command = Cli::parse().command;
    if let Some(Command::Daemon(DaemonCommand::Run { foreground })) = command {
        return match kprtfwd_daemon::run(kprtfwd_daemon::Options { foreground }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("kprtfwd daemon: {err:#}");
                ExitCode::FAILURE
            }
        };
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("kprtfwd: {err}");
            return ExitCode::FAILURE;
        }
    };
    match command {
        Some(command) => runtime.block_on(kprtfwd_cli::run(command)),
        None => match runtime.block_on(kprtfwd_tui::run()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("kprtfwd: {err:#}");
                ExitCode::FAILURE
            }
        },
    }
}
