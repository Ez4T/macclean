//! Applies the [`Ruleset`] to a directory tree to produce a [`Scan`] of
//! classified [`Item`]s. Anything large that no Rule matches becomes
//! `Unclassified` rather than being guessed at (the fail-safe of ADR-0001).
//!
//! The tree is walked exactly once with an fd-relative walker: each directory is
//! held open while its children are measured with `fstatat(AT_SYMLINK_NOFOLLOW)`
//! and child directories are opened with `openat`. That keeps per-entry stats
//! independent of path depth while preserving the strict depth-first stream the
//! classify fold consumes.

use crate::model::{Evidence, Item, RecoveryMethod, SafetyClass, Scan};
use crate::ruleset::{Match, Rule, Ruleset};
use crate::scan::on_disk_bytes_from_blocks;
use rayon::prelude::*;
use std::ffi::{CStr, CString, OsString};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// First Rule whose [`Match`] applies to `path`, in Ruleset order (user rules
/// first — see [`Ruleset::with_user_rules`]). Determines `is_dir` itself with a
/// stat; prefer [`match_rule_typed`] on a hot path where the file type is known.
pub fn match_rule<'a>(ruleset: &'a Ruleset, path: &Path) -> Option<&'a Rule> {
    match_rule_typed(ruleset, path, path.is_dir())
}

/// Like [`match_rule`] but with the file-type already known, avoiding an extra
/// stat per Item during a Scan.
pub fn match_rule_typed<'a>(ruleset: &'a Ruleset, path: &Path, is_dir: bool) -> Option<&'a Rule> {
    match_rule_index(ruleset, path, is_dir).map(|i| &ruleset.rules[i])
}

/// Index of the first matching Rule — the compact form the fd-relative walk
/// stores on each measured entry before the fold turns it into an Item.
fn match_rule_index(ruleset: &Ruleset, path: &Path, is_dir: bool) -> Option<usize> {
    let probe = PathRuleProbe { path };
    match_rule_index_with_probe(ruleset, path, is_dir, &probe)
}

fn match_rule_index_with_probe(
    ruleset: &Ruleset,
    path: &Path,
    is_dir: bool,
    probe: &impl MatchProbe,
) -> Option<usize> {
    ruleset
        .rules
        .iter()
        .position(|rule| matches_path(&rule.matches, path, is_dir, probe))
}

trait MatchProbe {
    fn sibling_exists(&self, sibling: &str) -> bool;
    fn child_exists(&self, child: &str) -> bool;
}

struct PathRuleProbe<'a> {
    path: &'a Path,
}

impl MatchProbe for PathRuleProbe<'_> {
    fn sibling_exists(&self, sibling: &str) -> bool {
        self.path
            .parent()
            .map(|p| p.join(sibling).exists())
            .unwrap_or(false)
    }

    fn child_exists(&self, child: &str) -> bool {
        self.path.join(child).exists()
    }
}

struct FdRuleProbe<'a> {
    path: &'a Path,
    parent_fd: Option<RawFd>,
    entry_name: Option<&'a CStr>,
    dir_fd: Option<RawFd>,
}

impl MatchProbe for FdRuleProbe<'_> {
    fn sibling_exists(&self, sibling: &str) -> bool {
        self.parent_fd
            .map(|fd| fstatat_name_exists(fd, sibling))
            .unwrap_or_else(|| {
                self.path
                    .parent()
                    .map(|p| p.join(sibling).exists())
                    .unwrap_or(false)
            })
    }

    fn child_exists(&self, child: &str) -> bool {
        if let Some(fd) = self.dir_fd {
            return fstatat_name_exists(fd, child);
        }
        if let (Some(parent_fd), Some(entry_name)) = (self.parent_fd, self.entry_name) {
            return fstatat_child_name_exists(parent_fd, entry_name, child);
        }
        self.path.join(child).exists()
    }
}

fn matches_path(m: &Match, path: &Path, is_dir: bool, probe: &impl MatchProbe) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match m {
        // The `Dir*` conditions only ever apply to directories.
        Match::DirNamed { dir: want } => is_dir && name == want,
        Match::DirBesideSibling { dir: want, sibling } => {
            is_dir && name == want && probe.sibling_exists(sibling)
        }
        Match::DirContainingFile { file } => is_dir && probe.child_exists(file),
        // Suffix conditions apply to files and directories alike.
        Match::PathSuffix { suffix } => path.to_string_lossy().ends_with(suffix.as_str()),
        Match::NameSuffix { suffix } => name.ends_with(suffix.as_str()),
        // A `*`/`?` glob over the Item's own name (issue #8).
        Match::NameGlob { pattern } => glob_match(pattern, name),
        // Combinators (issue #8): the file type is threaded through unchanged so
        // nested `Dir*` conditions still see whether the Item is a directory.
        Match::All { of } => of.iter().all(|m| matches_path(m, path, is_dir, probe)),
        Match::Any { of } => of.iter().any(|m| matches_path(m, path, is_dir, probe)),
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
///
/// One parallel pass: the fd-relative walker stats and matches every entry, and
/// the [`DirFrame`] fold turns the depth-first stream into Items without a
/// second syscall. Matched directories own their whole subtree (nested matches
/// pruned), and the largest match-free subtrees surface as `Unclassified`.
pub fn run(root: &Path, ruleset: &Ruleset, min_unclassified: u64) -> Scan {
    let mut items: Vec<Item> = Vec::new();
    let mut stack: Vec<DirFrame> = Vec::new();

    for entry in fd_relative_walk(root, ruleset) {
        let depth = entry.depth;
        let bytes = entry.bytes;
        let rule_idx = entry.rule_idx;
        let is_dir = entry.is_dir;

        // Strict depth-first order means every open frame at this depth or deeper
        // is now complete: pop and settle them (deepest first) before the entry.
        while stack.last().map(|f| f.depth >= depth).unwrap_or(false) {
            let frame = stack.pop().unwrap();
            finish_frame(frame, &mut stack, &mut items, ruleset, min_unclassified);
        }

        // After popping, the stack top is this entry's parent directory. Once an
        // ancestor has matched, everything below belongs to that Item.
        let covered = stack
            .last()
            .map(|f| f.owned || f.rule_idx.is_some())
            .unwrap_or(false);

        if is_dir {
            stack.push(DirFrame {
                path: entry.path,
                depth,
                bytes,
                rule_idx,
                owned: covered,
                has_match_below: rule_idx.is_some(),
                clean_children: Vec::new(),
            });
        } else {
            // A leaf folds its bytes into the open directory. A matched leaf that
            // isn't already inside an Item (e.g. a top-level `*.img.raw` image)
            // becomes an Item itself and marks its ancestors as holding a match.
            if let Some(top) = stack.last_mut() {
                top.bytes += bytes;
            }
            if let Some(ri) = rule_idx {
                if !covered {
                    items.push(make_item(entry.path, bytes, &ruleset.rules[ri]));
                    mark_match_below(&mut stack);
                }
            }
        }
    }

    // Drain the directories still open at end of stream, root last. Root is the
    // scan scope, never an Item, and always a descend point — so its match-free
    // children surface as the top-level unknowns (ADR-0001).
    while let Some(frame) = stack.pop() {
        finish_frame(frame, &mut stack, &mut items, ruleset, min_unclassified);
    }

    items.sort_by(|a, b| b.size_on_disk.cmp(&a.size_on_disk));
    Scan {
        root: root.to_path_buf(),
        items,
    }
}

/// A measured Item candidate in strict depth-first order. `path` is still needed
/// for display and suffix Rules, but metadata and marker probes were resolved
/// relative to open directory fds rather than by restatting this absolute path.
#[derive(Clone)]
struct WalkEntry {
    path: PathBuf,
    depth: usize,
    is_dir: bool,
    bytes: u64,
    rule_idx: Option<usize>,
}

struct ChildEntry {
    entry: WalkEntry,
    name: CString,
}

fn fd_relative_walk(root: &Path, ruleset: &Ruleset) -> Vec<WalkEntry> {
    let Some(root_stat) = lstat_path(root) else {
        return Vec::new();
    };

    let is_dir = mode_is_dir(root_stat.st_mode);
    let root_dir_fd = if is_dir { open_dir_path(root) } else { None };
    let root_rule_idx = {
        let probe = FdRuleProbe {
            path: root,
            parent_fd: None,
            entry_name: None,
            dir_fd: root_dir_fd.as_ref().map(AsRawFd::as_raw_fd),
        };
        match_rule_index_with_probe(ruleset, root, is_dir, &probe)
    };

    let mut entries = vec![WalkEntry {
        path: root.to_path_buf(),
        depth: 0,
        is_dir,
        bytes: stat_on_disk_bytes(&root_stat),
        rule_idx: root_rule_idx,
    }];

    if let Some(dir_fd) = root_dir_fd {
        entries.extend(walk_open_dir(root, 1, dir_fd, ruleset));
    }

    entries
}

fn walk_open_dir(
    dir_path: &Path,
    child_depth: usize,
    dir_fd: OwnedFd,
    ruleset: &Ruleset,
) -> Vec<WalkEntry> {
    let raw_fd = dir_fd.into_raw_fd();
    let dir = unsafe { libc::fdopendir(raw_fd) };
    if dir.is_null() {
        unsafe {
            libc::close(raw_fd);
        }
        return Vec::new();
    }

    let children = read_child_entries(dir, raw_fd, dir_path, child_depth, ruleset);
    let entries = walk_child_entries(raw_fd, children, ruleset);
    unsafe {
        libc::closedir(dir);
    }
    entries
}

fn read_child_entries(
    dir: *mut libc::DIR,
    raw_fd: RawFd,
    dir_path: &Path,
    child_depth: usize,
    ruleset: &Ruleset,
) -> Vec<ChildEntry> {
    let mut children = Vec::new();
    loop {
        let dirent = unsafe { libc::readdir(dir) };
        if dirent.is_null() {
            break;
        }

        let name = unsafe { CStr::from_ptr((*dirent).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }

        let Some(stat) = fstatat_no_follow(raw_fd, name) else {
            continue;
        };

        let is_dir = mode_is_dir(stat.st_mode);
        let child_path = dir_path.join(OsString::from_vec(name.to_bytes().to_vec()));
        let rule_idx = {
            let probe = FdRuleProbe {
                path: &child_path,
                parent_fd: Some(raw_fd),
                entry_name: Some(name),
                dir_fd: None,
            };
            match_rule_index_with_probe(ruleset, &child_path, is_dir, &probe)
        };

        children.push(ChildEntry {
            entry: WalkEntry {
                path: child_path,
                depth: child_depth,
                is_dir,
                bytes: stat_on_disk_bytes(&stat),
                rule_idx,
            },
            name: name.to_owned(),
        });
    }

    children
}

fn walk_child_entries(
    parent_fd: RawFd,
    children: Vec<ChildEntry>,
    ruleset: &Ruleset,
) -> Vec<WalkEntry> {
    const CHILD_DIR_OPEN_BATCH: usize = 256;

    let mut entries = Vec::new();
    for chunk in children.chunks(CHILD_DIR_OPEN_BATCH) {
        let work: Vec<(WalkEntry, Option<OwnedFd>)> = chunk
            .iter()
            .map(|child| {
                let child_dir_fd = if child.entry.is_dir {
                    open_dir_at(parent_fd, &child.name)
                } else {
                    None
                };
                (child.entry.clone(), child_dir_fd)
            })
            .collect();

        let subtrees: Vec<Vec<WalkEntry>> = work
            .into_par_iter()
            .map(|(entry, child_dir_fd)| {
                let child_path = entry.path.clone();
                let grandchild_depth = entry.depth + 1;
                let mut subtree = vec![entry];
                if let Some(child_dir_fd) = child_dir_fd {
                    subtree.extend(walk_open_dir(
                        &child_path,
                        grandchild_depth,
                        child_dir_fd,
                        ruleset,
                    ));
                }
                subtree
            })
            .collect();

        entries.extend(subtrees.into_iter().flatten());
    }

    entries
}

fn lstat_path(path: &Path) -> Option<libc::stat> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    fstatat_no_follow(libc::AT_FDCWD, &path)
}

fn fstatat_name_exists(dir_fd: RawFd, name: &str) -> bool {
    CString::new(name.as_bytes())
        .ok()
        .and_then(|name| fstatat_no_follow(dir_fd, &name))
        .is_some()
}

fn fstatat_child_name_exists(parent_fd: RawFd, entry_name: &CStr, child: &str) -> bool {
    let mut relative = Vec::with_capacity(entry_name.to_bytes().len() + 1 + child.len());
    relative.extend_from_slice(entry_name.to_bytes());
    relative.push(b'/');
    relative.extend_from_slice(child.as_bytes());

    CString::new(relative)
        .ok()
        .and_then(|name| fstatat_no_follow(parent_fd, &name))
        .is_some()
}

fn fstatat_no_follow(dir_fd: RawFd, name: &CStr) -> Option<libc::stat> {
    let mut stat = MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            dir_fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    (rc == 0).then(|| unsafe { stat.assume_init() })
}

fn open_dir_path(path: &Path) -> Option<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(path.as_ptr(), open_dir_flags()) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_dir_at(parent_fd: RawFd, name: &CStr) -> Option<OwnedFd> {
    let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), open_dir_flags()) };
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_dir_flags() -> libc::c_int {
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW
}

fn mode_is_dir(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR
}

fn stat_on_disk_bytes(stat: &libc::stat) -> u64 {
    on_disk_bytes_from_blocks(stat.st_blocks)
}

/// One open directory in the depth-first fold. The fd-relative walker yields
/// entries in strict depth-first order, so at most one frame per live depth sits
/// on the stack; a frame settles (is popped) the instant an entry at its depth
/// or shallower arrives, or when the stream ends.
struct DirFrame {
    path: PathBuf,
    depth: usize,
    /// Running on-disk total for this subtree: own blocks plus every descendant
    /// streamed so far. A completed child folds its total in when it pops.
    bytes: u64,
    /// First Rule this directory itself matched — it then owns its whole subtree
    /// as a single Item.
    rule_idx: Option<usize>,
    /// True when an ancestor already matched: this directory is part of that
    /// Item and is never emitted on its own (matched-item pruning).
    owned: bool,
    /// True once anything at or below here matched a Rule. Such a directory
    /// can't be surfaced whole — its match-free children are surfaced instead
    /// (issue #3), so a deeply nested unknown is still found.
    has_match_below: bool,
    /// Match-free child subtrees held as `Unclassified` candidates. They surface
    /// only if this directory turns out to contain a match (a descend point);
    /// otherwise the whole directory is one candidate and these are subsumed.
    clean_children: Vec<(PathBuf, u64)>,
}

/// Settle a popped directory: fold its bytes into its parent and decide what it
/// contributes — a matched Item, a set of surfaced `Unclassified` children, or a
/// single clean candidate handed up to its parent.
fn finish_frame(
    frame: DirFrame,
    stack: &mut [DirFrame],
    items: &mut Vec<Item>,
    ruleset: &Ruleset,
    min_unclassified: u64,
) {
    // `stack` now holds the ancestors; empty means this frame is the root.
    let is_root = stack.is_empty();
    if let Some(parent) = stack.last_mut() {
        parent.bytes += frame.bytes;
        if frame.has_match_below {
            parent.has_match_below = true;
        }
    }

    if let Some(ri) = frame.rule_idx {
        // A matched directory owns its subtree; only the outermost match becomes
        // an Item (nested ones are pruned), and its children are part of it.
        if !frame.owned && !is_root {
            items.push(make_item(frame.path, frame.bytes, &ruleset.rules[ri]));
        }
        return;
    }

    if frame.owned {
        // Inside a matched Item: not surfaced; its bytes already folded upward.
        return;
    }

    if is_root || frame.has_match_below {
        // A descend point: surface the match-free child subtrees that clear the
        // threshold as the unknowns nested beside matched data.
        for (path, bytes) in frame.clean_children {
            if bytes >= min_unclassified {
                items.push(unclassified_item(path, bytes));
            }
        }
    } else if let Some(parent) = stack.last_mut() {
        // Wholly match-free: hand the whole directory up as one candidate,
        // subsuming its own children so nested unknowns aren't double-counted.
        parent.clean_children.push((frame.path, frame.bytes));
    }
}

/// Mark every open directory as containing a match — used when a matched leaf is
/// emitted, since the live stack is exactly that leaf's chain of ancestors.
fn mark_match_below(stack: &mut [DirFrame]) {
    for frame in stack.iter_mut() {
        frame.has_match_below = true;
    }
}

fn make_item(path: PathBuf, size_on_disk: u64, rule: &Rule) -> Item {
    Item {
        size_on_disk,
        class: rule.class,
        recovery: recovery_for(rule),
        evidence: Evidence {
            summary: rule.evidence.clone(),
        },
        override_reclaim: false,
        path,
    }
}

fn unclassified_item(path: PathBuf, size_on_disk: u64) -> Item {
    Item {
        size_on_disk,
        class: SafetyClass::Unclassified,
        recovery: RecoveryMethod::None,
        evidence: Evidence {
            summary: "Large, but no Rule matched — inspect before deleting.".into(),
        },
        override_reclaim: false,
        path,
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

    /// Issue #19: deep trees should be handled by the fd-relative Scan path
    /// while preserving the Rule probes that need sibling and child markers.
    #[test]
    fn deep_tree_scan_matches_sibling_and_child_marker_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let mut deep = root.to_path_buf();
        for i in 0..32 {
            deep = deep.join(format!("{i:02x}"));
            fs::create_dir(&deep).unwrap();
        }

        let project = deep.join("project");
        let target = project.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(project.join("Cargo.toml"), b"[package]\n").unwrap();
        fs::write(target.join("artifact"), vec![0u8; 4096]).unwrap();

        let pgdata = deep.join("pgdata");
        fs::create_dir(&pgdata).unwrap();
        fs::write(pgdata.join("PG_VERSION"), b"16\n").unwrap();
        fs::write(pgdata.join("relation"), vec![0u8; 4096]).unwrap();

        let scan = run(root, &Ruleset::defaults(), 1024 * 1024 * 1024);

        assert_eq!(item_for(&scan, "target").class, SafetyClass::Regenerable);
        assert_eq!(item_for(&scan, "pgdata").class, SafetyClass::Irreplaceable);
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
            rules: vec![rule(
                "zst-archives",
                Match::NameGlob {
                    pattern: "*.zst".into(),
                },
            )],
        };
        // A single-file Item recognized by extension glob (issue #8 `*.zst` case).
        assert_eq!(
            match_rule_typed(&rs, Path::new("/d/backup.tar.zst"), false)
                .unwrap()
                .name,
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
                        Match::DirNamed {
                            dir: "build".into(),
                        },
                        Match::DirContainingFile {
                            file: "marker".into(),
                        },
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
        assert_eq!(
            match_rule_typed(&rs, &build, true).unwrap().name,
            "guarded-build"
        );
    }

    #[test]
    fn any_combinator_matches_either_spelling() {
        let rs = Ruleset {
            rules: vec![rule(
                "zstd-either",
                Match::Any {
                    of: vec![
                        Match::NameGlob {
                            pattern: "*.zst".into(),
                        },
                        Match::NameGlob {
                            pattern: "*.zstd".into(),
                        },
                    ],
                },
            )],
        };
        assert_eq!(
            match_rule_typed(&rs, Path::new("/a.zst"), false)
                .unwrap()
                .name,
            "zstd-either"
        );
        assert_eq!(
            match_rule_typed(&rs, Path::new("/a.zstd"), false)
                .unwrap()
                .name,
            "zstd-either"
        );
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

    /// A matched Item owns its whole subtree. A nested directory that would
    /// otherwise match another Rule must not be surfaced separately or counted
    /// twice.
    #[test]
    fn matched_item_prunes_nested_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let proj = root.join("proj");
        let target = proj.join("target");
        let nested_node_modules = target.join("node_modules");
        fs::create_dir_all(&nested_node_modules).unwrap();
        fs::write(proj.join("Cargo.toml"), b"[package]\n").unwrap();
        fs::write(target.join("build-output"), vec![0u8; 64 * 1024]).unwrap();
        fs::write(nested_node_modules.join("index.js"), vec![0u8; 64 * 1024]).unwrap();

        let scan = run(root, &Ruleset::defaults(), 1);

        let target_item = item_for(&scan, "target");
        assert_eq!(target_item.class, SafetyClass::Regenerable);
        assert!(
            scan.items
                .iter()
                .all(|i| i.path.file_name().and_then(|n| n.to_str()) != Some("node_modules")),
            "nested matches under an Item should be pruned"
        );
    }
}
