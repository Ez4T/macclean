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

    // Pass 2: surface the largest unmatched subtrees as Unclassified, recursing
    // past any directory that holds a matched Item so a deeply nested unknown is
    // still found even when its ancestors contain matched data (ADR-0001
    // fail-safe). Root's own matched children are skipped — they are already
    // Items.
    if let Ok(read) = std::fs::read_dir(root) {
        for child in read.filter_map(Result::ok) {
            let cpath = child.path();
            if !cpath.is_dir() {
                continue;
            }
            if matched_prefixes.iter().any(|p| p == &cpath) {
                continue;
            }
            surface_unclassified(&cpath, &matched_prefixes, min_unclassified, &mut items);
        }
    }

    items.sort_by(|a, b| b.size_on_disk.cmp(&a.size_on_disk));
    Scan { root: root.to_path_buf(), items }
}

/// Recursively surface the largest unmatched subtrees at or below `dir` as
/// `Unclassified` Items (ADR-0001). `dir` is always an unmatched directory.
///
/// - If `dir` contains *no* matched Item, the whole subtree is unknown, so it is
///   surfaced as a single Unclassified Item when it meets `min_unclassified` and
///   not descended into — this is what keeps nested unknowns from being double
///   counted against their parent.
/// - If `dir` *does* contain a matched Item, it can't be surfaced whole (the
///   matched parts aren't Unclassified), so we descend into its child
///   directories — skipping matched Items, which are pruned as their own units —
///   to surface the unmatched subtrees nested within.
fn surface_unclassified(
    dir: &Path,
    matched_prefixes: &[std::path::PathBuf],
    min_unclassified: u64,
    items: &mut Vec<Item>,
) {
    let has_matched_descendant = matched_prefixes
        .iter()
        .any(|p| p.starts_with(dir) && p != dir);

    if !has_matched_descendant {
        let size = on_disk_size(dir);
        if size >= min_unclassified {
            items.push(Item {
                size_on_disk: size,
                class: SafetyClass::Unclassified,
                recovery: RecoveryMethod::None,
                evidence: Evidence {
                    summary: "Large, but no Rule matched — inspect before deleting.".into(),
                },
                override_reclaim: false,
                path: dir.to_path_buf(),
            });
        }
        return;
    }

    if let Ok(read) = std::fs::read_dir(dir) {
        for child in read.filter_map(Result::ok) {
            let cpath = child.path();
            if !cpath.is_dir() {
                continue;
            }
            // A matched Item is its own unit — never folded into an Unclassified.
            if matched_prefixes.iter().any(|p| p == &cpath) {
                continue;
            }
            surface_unclassified(&cpath, matched_prefixes, min_unclassified, items);
        }
    }
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

    /// Acceptance for issue #3: a large unknown nested *below* a directory that
    /// also holds a matched Item is surfaced as a single Unclassified subtree.
    /// The old top-level-only Pass 2 skipped the whole branch and never found it.
    #[test]
    fn nested_unknown_under_a_matched_branch_is_surfaced() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // One top-level dir holding BOTH a matched Item and a nested unknown.
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).unwrap();

        // Matched: a Rust target/ beside a Cargo.toml (Regenerable).
        let proj = workspace.join("proj");
        fs::create_dir(&proj).unwrap();
        fs::write(proj.join("Cargo.toml"), b"[package]\n").unwrap();
        let target = proj.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("blob"), vec![0u8; 64 * 1024]).unwrap();

        // Unknown: a deeply nested dir no Rule matches, sitting beside `proj`.
        let bigdata = workspace.join("bigdata");
        let nested = bigdata.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("blob"), vec![0u8; 256 * 1024]).unwrap();

        let scan = run(root, &Ruleset::defaults(), 64 * 1024);

        // The matched target/ is still classified as its own unit.
        assert_eq!(item_for(&scan, "target").class, SafetyClass::Regenerable);

        // Exactly one Unclassified Item, and it is the largest clean subtree
        // (`bigdata`) — not its `nested` child (no descent past a clean tree),
        // and not `workspace` (which holds matched data, so it isn't surfaced
        // whole and isn't double-counted).
        let unclassified: Vec<&Item> = scan
            .items
            .iter()
            .filter(|i| i.class == SafetyClass::Unclassified)
            .collect();
        assert_eq!(unclassified.len(), 1, "exactly one Unclassified subtree");
        assert_eq!(
            unclassified[0].path.file_name().and_then(|n| n.to_str()),
            Some("bigdata"),
        );
        assert!(!unclassified[0].may_reclaim());
    }
}
