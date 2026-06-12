//! Reclaiming space. Two rules from the ADRs are enforced here:
//!
//! * **ADR-0002** — prefer the canonical clean tool (`cargo clean`, …), fall
//!   back to `rm` when no command exists or the tool is absent.
//! * **ADR-0004** — the four Reclaimable classes delete permanently; a manually
//!   overridden `Unclassified` Item goes to the Trash instead.

use crate::model::{Item, SafetyClass};
use crate::ruleset::Ruleset;
use crate::classify::match_rule;
use anyhow::{bail, Context, Result};
use std::process::Command;

/// What a reclaim actually did, for honest reporting back to the user.
#[derive(Debug)]
pub enum Reclaimed {
    ToolClean { command: String },
    Removed,
    Trashed,
}

/// Reclaim a single Item. Refuses anything not currently reclaimable
/// (Protected, or Unclassified without an override) — the Confirm gate lives in
/// the caller, but this is the last guardrail.
pub fn reclaim(item: &Item, ruleset: &Ruleset) -> Result<Reclaimed> {
    if !item.may_reclaim() {
        bail!(
            "{} is {} and not reclaimable",
            item.path.display(),
            item.class.label()
        );
    }

    // ADR-0004: overridden Unclassified → Trash (safety net), never permanent.
    if item.class == SafetyClass::Unclassified {
        trash::delete(&item.path)
            .with_context(|| format!("trashing {}", item.path.display()))?;
        return Ok(Reclaimed::Trashed);
    }

    // ADR-0002: prefer the canonical clean command if the matching Rule carries
    // one and the tool is on PATH; otherwise remove the directory directly.
    if let Some(rule) = match_rule(ruleset, &item.path) {
        if let Some(cmd) = &rule.clean_command {
            if tool_available(cmd) {
                run_clean(cmd, &item.path)?;
                return Ok(Reclaimed::ToolClean { command: cmd.clone() });
            }
        }
    }

    // A Reclaimable Item may be a file (e.g. a Redundant Copy of a disk image),
    // not only a directory tree, so remove whichever it is.
    if item.path.is_dir() {
        std::fs::remove_dir_all(&item.path)
            .with_context(|| format!("removing {}", item.path.display()))?;
    } else {
        std::fs::remove_file(&item.path)
            .with_context(|| format!("removing {}", item.path.display()))?;
    }
    Ok(Reclaimed::Removed)
}

/// First whitespace token of the command is the executable; check it resolves.
fn tool_available(command: &str) -> bool {
    let Some(exe) = command.split_whitespace().next() else {
        return false;
    };
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {exe}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the clean command in the Item's parent directory (where the toolchain
/// expects to find its project manifest).
fn run_clean(command: &str, item_path: &std::path::Path) -> Result<()> {
    let cwd = item_path
        .parent()
        .context("item has no parent directory")?;
    let status = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .status()
        .with_context(|| format!("running `{command}`"))?;
    if !status.success() {
        bail!("`{command}` exited with {status}");
    }
    Ok(())
}
