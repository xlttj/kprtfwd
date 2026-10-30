//! kprtfwd: Kubernetes port-forward manager.

use std::process::ExitCode;

use clap::Parser;
use kprtfwd_cli::{Command, DaemonCommand};

#[derive(Parser)]
#[command(name = "kprtfwd", version, about = "Kubernetes port-forward manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Daemon(DaemonCommand::Run { foreground }) => {
            match kprtfwd_daemon::run(kprtfwd_daemon::Options { foreground }) {
                Ok(()) => ExitCode::SUCCESS,
                Err(err) => {
                    eprintln!("kprtfwd daemon: {err:#}");
                    ExitCode::FAILURE
                }
            }
        }
        command => {
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
            runtime.block_on(kprtfwd_cli::run(command))
        }
    }
}
