//! kprtfwd v2 command line.
//!
//! So far only `db list`, which reads the database without changing it. The
//! TUI, the daemon and the remaining commands follow in later phases.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use kprtfwd_core::{ForwardId, Store, paths};

#[derive(Parser)]
#[command(name = "kprtfwd", version, about = "Kubernetes port-forward manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect the database directly, without the daemon
    #[command(subcommand)]
    Db(DbCommand),
}

#[derive(Subcommand)]
enum DbCommand {
    /// List forwards and projects; reads a copy, the file is not changed
    List {
        /// Database file [default: ~/.kprtfwd/kprtfwd.db]
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("kprtfwd: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Db(DbCommand::List { db }) => list(db),
    }
}

fn list(db: Option<PathBuf>) -> Result<()> {
    let path = match db {
        Some(path) => path,
        None => paths::default_db_path().context("HOME is not set")?,
    };
    if !path.exists() {
        bail!("no database at {}", path.display());
    }
    let store = Store::snapshot(&path).with_context(|| format!("read {}", path.display()))?;
    let forwards = store.forwards()?;
    let projects = store.projects()?;

    let mut member_of: HashMap<&ForwardId, Vec<&str>> = HashMap::new();
    for project in &projects {
        for id in &project.forwards {
            member_of.entry(id).or_default().push(&project.name);
        }
    }

    let rows: Vec<Vec<String>> = forwards
        .iter()
        .map(|f| {
            vec![
                if f.enabled { "on" } else { "off" }.to_owned(),
                f.context.clone(),
                format!("{}/{}:{}", f.namespace, f.service, f.remote_port),
                f.local_port.to_string(),
                member_of
                    .get(&f.id)
                    .map(|names| names.join(","))
                    .unwrap_or_default(),
                f.id.to_string(),
            ]
        })
        .collect();
    print_table(
        &["STATE", "CONTEXT", "SERVICE", "LOCAL", "PROJECTS", "ID"],
        &rows,
    );

    if !projects.is_empty() {
        println!();
        let rows: Vec<Vec<String>> = projects
            .iter()
            .map(|p| vec![p.name.clone(), p.forwards.len().to_string()])
            .collect();
        print_table(&["PROJECT", "FORWARDS"], &rows);
    }

    let mut contexts: Vec<&str> = forwards.iter().map(|f| f.context.as_str()).collect();
    contexts.dedup();
    println!(
        "\n{}, {}, {} (read from {})",
        count(forwards.len(), "forward"),
        count(contexts.len(), "context"),
        count(projects.len(), "project"),
        path.display()
    );
    Ok(())
}

fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Prints left-aligned columns separated by two spaces.
fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let last = cells.len().saturating_sub(1);
        let mut out = String::new();
        for (i, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            if i == last {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  "));
            }
        }
        println!("{}", out.trim_end());
    };
    line(header.to_vec());
    for row in rows {
        line(row.iter().map(String::as_str).collect());
    }
}
