//! Applies the [`Ruleset`] to a directory tree to produce a [`Scan`] of
//! classified [`Item`]s. Anything large that no Rule matches becomes
//! `Unclassified` rather than being guessed at (the fail-safe of ADR-0001).

use crate::model::{Evidence, Item, RecoveryMethod, SafetyClass, Scan};
use crate::ruleset::{Match, Rule, Ruleset};
use crate::scan::on_disk_size;
use jwalk::WalkDir;
use std::path::Path;

/// First Rule whose [`Match`] applies to `path`, in Ruleset order (user rules
/// first — see [`Ruleset::with_user_rules`]). Determines `is_dir` itself with a
/// stat; prefer [`match_rule_typed`] on a hot path where the file type is known.
pub fn match_rule<'a>(ruleset: &'a Ruleset, path: &Path) -> Option<&'a Rule> {
    match_rule_typed(ruleset, path, path.is_dir())
}

/// Like [`match_rule`] but with the file-type already known, avoiding an extra
/// stat per Item during a Scan.
pub fn match_rule_typed<'a>(ruleset: &'a Ruleset, path: &Path, is_dir: bool) -> Option<&'a Rule> {
    ruleset
        .rules
        .iter()
        .find(|rule| matches_path(&rule.matches, path, is_dir))
}

fn matches_path(m: &Match, path: &Path, is_dir: bool) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match m {
        // The `Dir*` conditions only ever apply to directories.
        Match::DirNamed { dir: want } => is_dir && name == want,
        Match::DirBesideSibling { dir: want, sibling } => {
            is_dir
                && name == want
                && path
                    .parent()
                    .map(|p| p.join(sibling).exists())
                    .unwrap_or(false)
        }
        Match::DirContainingFile { file } => is_dir && path.join(file).exists(),
        // Suffix conditions apply to files and directories alike.
        Match::PathSuffix { suffix } => path.to_string_lossy().ends_with(suffix.as_str()),
        Match::NameSuffix { suffix } => name.ends_with(suffix.as_str()),
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

    // Pass 1: find every matched Item, pruning anything inside one already
    // matched (a matched Item is reclaimed — or protected — as one unit). Both
    // directories (e.g. `target/`) and files (e.g. a `*.img.raw` VM image) can
    // match, so we consider every entry rather than directories alone.
    for entry in WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();
        let is_dir = entry.file_type().is_dir();
        // Skip anything already inside a matched Item.
        if matched_prefixes.iter().any(|p| path.starts_with(p)) {
            continue;
        }
        if let Some(rule) = match_rule_typed(ruleset, &path, is_dir) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn item_for<'a>(scan: &'a Scan, name: &str) -> &'a Item {
        scan.items
            .iter()
            .find(|i| i.path.file_name().and_then(|n| n.to_str()) == Some(name))
            .unwrap_or_else(|| panic!("no Item named {name} in scan"))
    }

    /// Acceptance for issue #1: a Scan over a tree containing a VM image and a
    /// Postgres data dir labels them Irreplaceable (Protected), and the
    /// reclaimable total excludes them while still counting genuine Reclaimables.
    #[test]
    fn vm_image_and_postgres_dir_are_irreplaceable_and_not_reclaimable() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // A VM disk image — a file, not a directory.
        fs::write(root.join("orbstack.img.raw"), vec![0u8; 4096]).unwrap();

        // A PostgreSQL data directory, recognized by its PG_VERSION marker.
        let pg = root.join("pgdata");
        fs::create_dir(&pg).unwrap();
        fs::write(pg.join("PG_VERSION"), b"16\n").unwrap();
        fs::create_dir(pg.join("base")).unwrap();
        fs::write(pg.join("base").join("1247"), vec![0u8; 4096]).unwrap();

        // A genuine Reclaimable, so we can prove the total still counts those.
        let nm = root.join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("index.js"), vec![0u8; 4096]).unwrap();

        // High threshold so nothing falls through to Unclassified and muddies the
        // assertions — every Item here is matched by a Rule.
        let scan = run(root, &Ruleset::defaults(), 1024 * 1024 * 1024);

        let vm = item_for(&scan, "orbstack.img.raw");
        assert_eq!(vm.class, SafetyClass::Irreplaceable);
        assert!(vm.class.is_protected());
        assert!(!vm.may_reclaim());

        let pgdata = item_for(&scan, "pgdata");
        assert_eq!(pgdata.class, SafetyClass::Irreplaceable);
        assert!(!pgdata.may_reclaim());
        // The data dir is matched as one unit; `base/` is not surfaced separately.
        assert!(
            scan.items
                .iter()
                .all(|i| i.path.file_name().and_then(|n| n.to_str()) != Some("base")),
            "matched Postgres dir should prune its children"
        );

        let node_modules = item_for(&scan, "node_modules");
        assert!(node_modules.may_reclaim());

        // Reclaimable total counts node_modules but neither Irreplaceable Item.
        assert_eq!(scan.reclaimable_bytes(), node_modules.size_on_disk);
    }
}
