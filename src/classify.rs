//! Applies the [`Ruleset`] to a directory tree to produce a [`Scan`] of
//! classified [`Item`]s. Anything large that no Rule matches becomes
//! `Unclassified` rather than being guessed at (the fail-safe of ADR-0001).

use crate::model::{Evidence, Item, RecoveryMethod, SafetyClass, Scan};
use crate::ruleset::{Match, Rule, Ruleset};
use crate::scan::on_disk_size;
use jwalk::WalkDir;
use std::path::Path;

/// First Rule whose [`Match`] applies to `dir`, in Ruleset order (user rules
/// first — see [`Ruleset::with_user_rules`]).
pub fn match_rule<'a>(ruleset: &'a Ruleset, dir: &Path) -> Option<&'a Rule> {
    ruleset.rules.iter().find(|rule| matches_dir(&rule.matches, dir))
}

fn matches_dir(m: &Match, dir: &Path) -> bool {
    let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match m {
        Match::DirNamed { dir: want } => name == want,
        Match::DirBesideSibling { dir: want, sibling } => {
            name == want
                && dir
                    .parent()
                    .map(|p| p.join(sibling).exists())
                    .unwrap_or(false)
        }
        Match::PathSuffix { suffix } => dir.to_string_lossy().ends_with(suffix.as_str()),
    }
}

fn recovery_for(rule: &Rule) -> RecoveryMethod {
    match rule.class {
        SafetyClass::Regenerable => RecoveryMethod::Rebuild {
            command: rule.recover_command.clone().unwrap_or_default(),
        },
        SafetyClass::Reinstallable => RecoveryMethod::Reinstall {
            command: rule.recover_command.clone().unwrap_or_default(),
        },
        SafetyClass::Cache => RecoveryMethod::AutoRefill,
        _ => RecoveryMethod::None,
    }
}

/// Walk `root`, classify directories against `ruleset`, and surface anything
/// large-but-unmatched as `Unclassified`. `min_unclassified` is the on-disk
/// threshold below which unmatched dirs are ignored (kept out of the way).
pub fn run(root: &Path, ruleset: &Ruleset, min_unclassified: u64) -> Scan {
    let mut items: Vec<Item> = Vec::new();
    let mut matched_prefixes: Vec<std::path::PathBuf> = Vec::new();

    // Pass 1: find every matched directory, deepest-first prune (we don't
    // descend into a matched dir — it is reclaimed as one unit).
    for entry in WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if !entry.file_type().is_dir() {
            continue;
        }
        // Skip anything already inside a matched Item.
        if matched_prefixes.iter().any(|p| path.starts_with(p)) {
            continue;
        }
        if let Some(rule) = match_rule(ruleset, &path) {
            items.push(Item {
                size_on_disk: on_disk_size(&path),
                class: rule.class,
                recovery: recovery_for(rule),
                evidence: Evidence { summary: rule.evidence.clone() },
                override_reclaim: false,
                path: path.clone(),
            });
            matched_prefixes.push(path);
        }
    }

    // Pass 2: top-level children of root that are large, unmatched, and contain
    // no matched Item become Unclassified (ADR-0001 fail-safe).
    if let Ok(read) = std::fs::read_dir(root) {
        for child in read.filter_map(Result::ok) {
            let cpath = child.path();
            if !cpath.is_dir() {
                continue;
            }
            let is_matched = matched_prefixes.iter().any(|p| p == &cpath);
            let has_matched_descendant =
                matched_prefixes.iter().any(|p| p.starts_with(&cpath) && p != &cpath);
            if is_matched || has_matched_descendant {
                continue;
            }
            let size = on_disk_size(&cpath);
            if size >= min_unclassified {
                items.push(Item {
                    size_on_disk: size,
                    class: SafetyClass::Unclassified,
                    recovery: RecoveryMethod::None,
                    evidence: Evidence {
                        summary: "Large, but no Rule matched — inspect before deleting.".into(),
                    },
                    override_reclaim: false,
                    path: cpath,
                });
            }
        }
    }

    items.sort_by(|a, b| b.size_on_disk.cmp(&a.size_on_disk));
    Scan { root: root.to_path_buf(), items }
}
