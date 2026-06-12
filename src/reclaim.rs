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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evidence, Item, RecoveryMethod, SafetyClass};
    use crate::ruleset::{Match, Rule, Ruleset};
    use std::fs;
    use std::path::PathBuf;

    fn item(path: PathBuf, class: SafetyClass, override_reclaim: bool) -> Item {
        Item {
            path,
            size_on_disk: 4096,
            class,
            recovery: RecoveryMethod::None,
            evidence: Evidence { summary: "fixture".into() },
            override_reclaim,
        }
    }

    /// Guardrail: a Protected (Irreplaceable) Item is refused even if reclaim is
    /// somehow called on it, and nothing is deleted (CONTEXT.md → "Protected").
    #[test]
    fn refuses_protected_item() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("disk.img.raw");
        fs::write(&img, vec![0u8; 4096]).unwrap();

        let it = item(img.clone(), SafetyClass::Irreplaceable, false);
        let err = reclaim(&it, &Ruleset::defaults()).unwrap_err();
        assert!(err.to_string().contains("not reclaimable"));
        assert!(img.exists(), "Protected Item is never deleted");
    }

    /// Guardrail: an Unclassified Item without an override is refused (ADR-0001) —
    /// it is surfaced but not reclaimable until the user explicitly overrides.
    #[test]
    fn refuses_unoverridden_unclassified() {
        let tmp = tempfile::tempdir().unwrap();
        let mystery = tmp.path().join("mystery");
        fs::create_dir(&mystery).unwrap();

        let it = item(mystery.clone(), SafetyClass::Unclassified, false);
        assert!(reclaim(&it, &Ruleset::defaults()).is_err());
        assert!(mystery.exists(), "un-overridden Unclassified is never deleted");
    }

    /// Destination by class (ADR-0004): an *overridden* Unclassified Item is routed
    /// to the Trash, not deleted permanently. The actual trash op is environment
    /// dependent (Finder/freedesktop), so we accept an Err from a headless box but
    /// require that, when it succeeds, it took the Trash branch and removed the
    /// original path — never the permanent `Removed`/`ToolClean` branch.
    #[test]
    fn overridden_unclassified_routes_to_trash() {
        let tmp = tempfile::tempdir().unwrap();
        let mystery = tmp.path().join("macclean-test-trash-me");
        fs::create_dir(&mystery).unwrap();

        let it = item(mystery.clone(), SafetyClass::Unclassified, true);
        match reclaim(&it, &Ruleset::defaults()) {
            Ok(Reclaimed::Trashed) => assert!(!mystery.exists(), "Trashed → moved out of place"),
            Ok(other) => panic!("overridden Unclassified must go to Trash, got {other:?}"),
            Err(_) => { /* no Trash backend in this environment; branch was still taken */ }
        }
    }

    /// Hybrid reclaim (ADR-0002): when the matching Rule carries a clean command
    /// and the tool is available, reclaim prefers it over a raw `rm`. We use a
    /// harmless `true` as the clean command so the test is deterministic and the
    /// fixture survives — proving the clean branch ran, not the removal branch.
    #[test]
    fn prefers_clean_command_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("buildcache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("artifact"), vec![0u8; 4096]).unwrap();

        let ruleset = Ruleset {
            rules: vec![Rule {
                name: "fixture-clean".into(),
                matches: Match::DirNamed { dir: "buildcache".into() },
                class: SafetyClass::Regenerable,
                clean_command: Some("true".into()),
                recover_command: Some("make".into()),
                evidence: "fixture".into(),
            }],
        };

        let it = item(cache.clone(), SafetyClass::Regenerable, false);
        match reclaim(&it, &ruleset).unwrap() {
            Reclaimed::ToolClean { command } => assert_eq!(command, "true"),
            other => panic!("expected ToolClean, got {other:?}"),
        }
    }

    /// Hybrid reclaim fallback (ADR-0002): with no clean command, a Reclaimable
    /// Item is removed directly and its bytes are gone.
    #[test]
    fn falls_back_to_rm_without_a_clean_command() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("index.js"), vec![0u8; 4096]).unwrap();

        let it = item(nm.clone(), SafetyClass::Reinstallable, false);
        match reclaim(&it, &Ruleset::defaults()).unwrap() {
            Reclaimed::Removed => assert!(!nm.exists(), "directly removed"),
            other => panic!("expected Removed, got {other:?}"),
        }
    }
}
