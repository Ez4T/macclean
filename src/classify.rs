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
        // A `*`/`?` glob over the Item's own name (issue #8).
        Match::NameGlob { pattern } => glob_match(pattern, name),
        // Combinators (issue #8): the file type is threaded through unchanged so
        // nested `Dir*` conditions still see whether the Item is a directory.
        Match::All { of } => of.iter().all(|m| matches_path(m, path, is_dir)),
        Match::Any { of } => of.iter().any(|m| matches_path(m, path, is_dir)),
    }
}

/// Shell-style glob match supporting `*` (any run, including empty) and `?` (any
/// single character); every other character is literal. Used by
/// [`Match::NameGlob`]. A small two-pointer matcher so the Ruleset needs no glob
/// crate and stays pure data (ADR-0003).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // `star` remembers the last `*` so we can backtrack; `mark` is where in the
    // text that `*` is currently assumed to stop consuming.
    let (mut star, mut mark) = (None, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Mismatch under a `*`: let the `*` swallow one more character.
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    // Trailing `*`s in the pattern can still match the empty remainder.
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
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

    // --- Richer Match conditions (issue #8) ---

    use crate::ruleset::Match;

    fn rule(name: &str, matches: Match) -> Rule {
        Rule {
            name: name.into(),
            matches,
            class: SafetyClass::Cache,
            clean_command: None,
            recover_command: None,
            evidence: "fixture".into(),
        }
    }

    #[test]
    fn glob_match_handles_star_and_question() {
        assert!(glob_match("*.zst", "archive.tar.zst"));
        assert!(glob_match("core.*", "core.12345"));
        assert!(glob_match("?cache", "Xcache"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(glob_match("exact", "exact"));

        assert!(!glob_match("*.zst", "archive.tar.gz"));
        assert!(!glob_match("?cache", "cache")); // ? needs exactly one char
        assert!(!glob_match("core.*", "core")); // literal dot must be present
        assert!(!glob_match("a*c", "abx"));
    }

    #[test]
    fn name_glob_rule_matches_by_wildcard() {
        let rs = Ruleset {
            rules: vec![rule("zst-archives", Match::NameGlob { pattern: "*.zst".into() })],
        };
        // A single-file Item recognized by extension glob (issue #8 `*.zst` case).
        assert_eq!(
            match_rule_typed(&rs, Path::new("/d/backup.tar.zst"), false).unwrap().name,
            "zst-archives",
        );
        assert!(match_rule_typed(&rs, Path::new("/d/backup.tar.gz"), false).is_none());
    }

    #[test]
    fn all_combinator_requires_every_condition() {
        // Match only a directory named `build` that also contains a `marker` file.
        let rs = Ruleset {
            rules: vec![rule(
                "guarded-build",
                Match::All {
                    of: vec![
                        Match::DirNamed { dir: "build".into() },
                        Match::DirContainingFile { file: "marker".into() },
                    ],
                },
            )],
        };

        let tmp = tempfile::tempdir().unwrap();
        let build = tmp.path().join("build");
        fs::create_dir(&build).unwrap();

        // Name matches but the marker is missing → AND fails.
        assert!(match_rule_typed(&rs, &build, true).is_none());

        // Add the marker → both conditions hold.
        fs::write(build.join("marker"), b"x").unwrap();
        assert_eq!(match_rule_typed(&rs, &build, true).unwrap().name, "guarded-build");
    }

    #[test]
    fn any_combinator_matches_either_spelling() {
        let rs = Ruleset {
            rules: vec![rule(
                "zstd-either",
                Match::Any {
                    of: vec![
                        Match::NameGlob { pattern: "*.zst".into() },
                        Match::NameGlob { pattern: "*.zstd".into() },
                    ],
                },
            )],
        };
        assert_eq!(match_rule_typed(&rs, Path::new("/a.zst"), false).unwrap().name, "zstd-either");
        assert_eq!(match_rule_typed(&rs, Path::new("/a.zstd"), false).unwrap().name, "zstd-either");
        assert!(match_rule_typed(&rs, Path::new("/a.gz"), false).is_none());
    }

    /// Mirrors the `/tmp` smoke test (issue #6): a matched Reinstallable plus a
    /// large unknown sibling. The unknown surfaces as Unclassified (ADR-0001
    /// fail-safe) and is excluded from the reclaimable total, which counts only
    /// the node_modules.
    #[test]
    fn matched_and_unknown_split_into_reclaimable_and_unclassified() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Matched: a node_modules tree → Reinstallable.
        let nm = root.join("proj").join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        fs::write(nm.join("index.js"), vec![0u8; 128 * 1024]).unwrap();

        // Unknown: a large dir no Rule matches → Unclassified.
        let mystery = root.join("mystery");
        fs::create_dir(&mystery).unwrap();
        fs::write(mystery.join("blob"), vec![0u8; 256 * 1024]).unwrap();

        let scan = run(root, &Ruleset::defaults(), 64 * 1024);

        let node_modules = item_for(&scan, "node_modules");
        assert_eq!(node_modules.class, SafetyClass::Reinstallable);
        assert!(node_modules.may_reclaim());

        let mystery_item = item_for(&scan, "mystery");
        assert_eq!(mystery_item.class, SafetyClass::Unclassified);
        assert!(
            !mystery_item.may_reclaim(),
            "Unclassified is surfaced but not offered (ADR-0001)",
        );

        // Reclaimable total counts the Reinstallable, never the Unclassified.
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
