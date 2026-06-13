//! macclean — surface disk usage, classify each Item by Safety Class, and
//! reclaim space only after an explicit Confirm. See CONTEXT.md for the
//! vocabulary and docs/adr/ for the decisions behind the design.

mod classify;
mod dedup;
mod model;
mod reclaim;
mod ruleset;
mod scan;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ruleset::Ruleset;
use scan::human;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "macclean", version, about = "Classify and reclaim disk usage, safely.")]
struct Cli {
    /// Root to scan. Defaults to $HOME (full-disk requires --full-disk).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Scan the whole disk from `/` instead of $HOME. May need elevated access.
    #[arg(long, global = true)]
    full_disk: bool,

    /// Ignore unmatched directories smaller than this many MB when surfacing
    /// Unclassified items.
    #[arg(long, global = true, default_value_t = 200)]
    min_unclassified_mb: u64,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a Scan and print the classified Items without launching the TUI.
    Scan,
}

fn resolve_root(cli: &Cli) -> Result<PathBuf> {
    if cli.full_disk {
        return Ok(PathBuf::from("/"));
    }
    if let Some(r) = &cli.root {
        return Ok(r.clone());
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; pass --root")
}

fn load_ruleset() -> Ruleset {
    let base = Ruleset::defaults();
    // Optional user rules: ~/.config/macclean/rules.toml (ADR-0003).
    let user_path =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/macclean/rules.toml"));
    if let Some(p) = user_path {
        if let Ok(text) = std::fs::read_to_string(&p) {
            match base.clone().with_user_rules(&text) {
                Ok(merged) => return merged,
                Err(e) => eprintln!("warning: ignoring {}: {e}", p.display()),
            }
        }
    }
    base
}

fn run_scan(root: &Path, ruleset: &Ruleset, min: u64) -> model::Scan {
    let mut scan = classify::run(root, ruleset, min);
    // Second pass: relabel byte-identical duplicates as Redundant Copy so the
    // surviving original is the one kept (CONTEXT.md → "Redundant Copy").
    dedup::analyze(&mut scan);
    scan
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = resolve_root(&cli)?;
    let ruleset = load_ruleset();
    let min = cli.min_unclassified_mb * 1024 * 1024;

    match cli.command {
        Some(Command::Scan) => {
            let scan = run_scan(&root, &ruleset, min);
            println!("Scan of {}", scan.root.display());
            for item in &scan.items {
                println!(
                    "  {:>8}  {:<14}  {}",
                    human(item.size_on_disk),
                    item.class.label(),
                    item.path.display()
                );
            }
            println!("Reclaimable now: {}", human(scan.reclaimable_bytes()));
            Ok(())
        }
        None => tui::run_scanning(root, ruleset, min),
    }
}
