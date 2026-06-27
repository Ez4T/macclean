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
        SafetyClass::Cache | SafetyClass::BrowserCache => RecoveryMethod::AutoRefill,
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
    // Phase 1: sequential readdir — names only, no syscall per entry.
    let mut names: Vec<CString> = Vec::new();
    loop {
        let dirent = unsafe { libc::readdir(dir) };
        if dirent.is_null() {
            break;
        }
        let name = unsafe { CStr::from_ptr((*dirent).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        names.push(name.to_owned());
    }

    // Phase 2: parallel fstatat + rule matching — one syscall per entry but
    // all entries run concurrently across rayon workers. For flat dirs with
    // many files (e.g. ~/Library, a large node_modules) this is the hot path.
    names
        .into_par_iter()
        .filter_map(|name| {
            let stat = fstatat_no_follow(raw_fd, &name)?;
            let is_dir = mode_is_dir(stat.st_mode);
            let child_path = dir_path.join(OsString::from_vec(name.to_bytes().to_vec()));
            let rule_idx = {
                let probe = FdRuleProbe {
                    path: &child_path,
                    parent_fd: Some(raw_fd),
                    entry_name: Some(&name),
                    dir_fd: None,
                };
                match_rule_index_with_probe(ruleset, &child_path, is_dir, &probe)
            };
            Some(ChildEntry {
                entry: WalkEntry {
                    path: child_path,
                    depth: child_depth,
                    is_dir,
                    bytes: entry_on_disk_bytes(raw_fd, &name, &stat),
                    rule_idx,
                },
                name,
            })
        })
        .collect()
}

fn walk_child_entries(
    parent_fd: RawFd,
    children: Vec<ChildEntry>,
    ruleset: &Ruleset,
) -> Vec<WalkEntry> {
    const CHILD_DIR_OPEN_BATCH: usize = 256;

    let mut entries = Vec::new();
    for chunk in children.chunks(CHILD_DIR_OPEN_BATCH) {
        // For each child resolve to: a leaf entry (no fd) or an open fd to
        // recurse into. A matched directory skips recursion entirely — its
        // total subtree size comes from getattrlist in one syscall instead of
        // statting every file inside (falls back to open+recurse on error).
        let work: Vec<(WalkEntry, Option<OwnedFd>)> = chunk
            .par_iter()
            .map(|child| {
                if child.entry.is_dir && child.entry.rule_idx.is_some() {
                    // Matched dir: skip the full recursive walk entirely.
                    // Try getattrlist first (one syscall, works on HFS+).
                    // Fall back to a rule-free jwalk byte count on filesystems
                    // that don't support ATTR_DIR_ALLOCSIZE (e.g. APFS) — still
                    // avoids rule-matching overhead for every file in the subtree.
                    let total = getattrlist_total_size(parent_fd, &child.name)
                        .unwrap_or_else(|| fast_tree_bytes(&child.entry.path));
                    let mut entry = child.entry.clone();
                    entry.bytes = total;
                    return (entry, None);
                }
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

/// Get the total allocated size of a directory tree via `getattrlistat` in a
/// single syscall, using `ATTR_DIR_ALLOCSIZE` (dirattr=0x8). This works on HFS+
/// but returns 0 on APFS, where there is no single-syscall recursive total — the
/// caller then falls back to the rule-free `fast_tree_bytes` walk (ADR-0006).
///
/// There is deliberately no `ATTR_CMN_*` size attempt here: macOS exposes no
/// `ATTR_CMN_TOTALSIZE`. An earlier version requested commonattr `0x400`, which
/// is actually `ATTR_CMN_MODTIME`, so every matched directory reported the
/// current Unix timestamp (~1.7 GiB) as its size and the reclaimable total
/// ballooned to impossible figures (issue #43).
fn getattrlist_total_size(parent_fd: RawFd, name: &CStr) -> Option<u64> {
    // dirattr = ATTR_DIR_ALLOCSIZE (0x8). A directory with no recursive size
    // returns 0 here (APFS), which we treat as "unavailable" so the caller falls
    // back to the rule-free walk — hence the `n > 0` filter.
    getattrlistat_off_t(parent_fd, name, 0, 0x0000_0008, 0, 0, 0)
        .and_then(|n| u64::try_from(n).ok())
        .filter(|&n| n > 0)
}

/// Bytes unique to a single file via `getattrlistat(ATTR_CMNEXT_PRIVATESIZE)`
/// (forkattr 0x8) — what deleting the file would actually free, excluding blocks
/// shared with APFS clones via copy-on-write (issue #47). Returns `None` when the
/// attribute is unavailable (older macOS, non-APFS, error) so the caller falls
/// back to allocated-block sizing and never regresses below today's behaviour.
///
/// A legitimate private size of 0 (a fully-shared clone, every block referenced
/// by another file) is distinguished from an absent attribute via the buffer's
/// leading length word, so such a file is correctly sized at ~0 rather than
/// falling back to its full allocation.
fn getattrlistat_private_size(parent_fd: RawFd, name: &CStr) -> Option<u64> {
    // Options: FSOPT_ATTR_CMN_EXTENDED (0x20) is *required* for the forkattr
    // field to be read as extended common attributes (ATTR_CMNEXT_*) — without it
    // getattrlistat rejects the request with EINVAL. FSOPT_NOFOLLOW (0x1) never
    // traverses a symlink swapped in under us; we only call this for entries
    // already known to be regular files, but it costs nothing and closes the
    // TOCTOU gap.
    getattrlistat_off_t(parent_fd, name, 0, 0, 0, 0x0000_0008, 0x0000_0021)
        .and_then(|n| u64::try_from(n).ok())
}

/// One `getattrlistat` requesting a single `off_t` attribute. Returns the value
/// only when the attribute was actually populated — determined from the buffer's
/// leading length word, so a real 0 is kept and an absent attribute yields
/// `None`. `commonattr`/`dirattr`/`fileattr`/`forkattr` select the one attribute;
/// `options` is the `getattrlist` option mask (e.g. `FSOPT_NOFOLLOW`).
fn getattrlistat_off_t(
    parent_fd:  RawFd,
    name:       &CStr,
    commonattr: u32,
    dirattr:    u32,
    fileattr:   u32,
    forkattr:   u32,
    options:    libc::c_ulong,
) -> Option<i64> {
    extern "C" {
        fn getattrlistat(
            fd:          libc::c_int,
            path:        *const libc::c_char,
            attr_list:   *mut libc::c_void,
            attr_buf:    *mut libc::c_void,
            attr_buf_sz: libc::size_t,
            options:     libc::c_ulong,
        ) -> libc::c_int;
    }

    #[repr(C)]
    struct AttrList {
        bitmapcount: u16,
        reserved:    u16,
        commonattr:  u32,
        volattr:     u32,
        dirattr:     u32,
        fileattr:    u32,
        forkattr:    u32,
    }

    // getattrlist returns a 4-byte u32 total-length prefix followed by the
    // requested attributes packed at 4-byte boundaries. For a single off_t that
    // is [len:u32][value:off_t] = 12 bytes when present, or just [len:u32] = 4
    // bytes when the filesystem does not supply the attribute.
    let mut buf = [0u8; 12];

    let mut al = AttrList {
        bitmapcount: 5,
        reserved:    0,
        commonattr,
        volattr:     0,
        dirattr,
        fileattr,
        forkattr,
    };

    let rc = unsafe {
        getattrlistat(
            parent_fd,
            name.as_ptr(),
            &mut al as *mut AttrList as *mut libc::c_void,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            options,
        )
    };

    if rc != 0 {
        return None;
    }

    // The leading length word counts itself; < 12 means the off_t was not
    // written, i.e. the attribute is unavailable on this filesystem.
    let len = u32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    if len < 12 {
        return None;
    }
    Some(i64::from_ne_bytes(buf[4..12].try_into().ok()?))
}

/// Rule-free recursive byte count using the same parallel fd-relative walker as
/// the main scan, but without rule matching. Used as the fallback when
/// `getattrlist_total_size` is unavailable (e.g. on APFS). Parallelises both
/// the fstatat calls within each directory and the recursion across siblings,
/// so it saturates all rayon workers rather than staying single-threaded.
fn fast_tree_bytes(path: &Path) -> u64 {
    let Some(root_stat) = lstat_path(path) else {
        return 0;
    };
    // The matched root may itself be a regular (cloned) file, so size it
    // clone-aware too — AT_FDCWD + the full path is the fd-relative form.
    let own = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => entry_on_disk_bytes(libc::AT_FDCWD, &c, &root_stat),
        Err(_) => stat_on_disk_bytes(&root_stat),
    };
    if !mode_is_dir(root_stat.st_mode) {
        return own;
    }
    let Some(dir_fd) = open_dir_path(path) else {
        return own;
    };
    own + fast_dir_bytes(dir_fd)
}

/// Parallel fd-relative byte-counter for an already-open directory.
/// Sequential readdir collects names; parallel fstatat + recursive descent
/// measures sizes across all rayon workers. Mirrors `walk_child_entries` but
/// returns a plain u64 instead of a WalkEntry stream.
fn fast_dir_bytes(dir_fd: OwnedFd) -> u64 {
    let raw_fd = dir_fd.into_raw_fd();
    let dir = unsafe { libc::fdopendir(raw_fd) };
    if dir.is_null() {
        unsafe { libc::close(raw_fd); }
        return 0;
    }

    let mut names: Vec<CString> = Vec::new();
    loop {
        let dirent = unsafe { libc::readdir(dir) };
        if dirent.is_null() { break; }
        let name = unsafe { CStr::from_ptr((*dirent).d_name.as_ptr()) };
        if matches!(name.to_bytes(), b"." | b"..") { continue; }
        names.push(name.to_owned());
    }

    let total: u64 = names
        .into_par_iter()
        .map(|name| {
            let Some(stat) = fstatat_no_follow(raw_fd, &name) else { return 0; };
            let own = entry_on_disk_bytes(raw_fd, &name, &stat);
            if mode_is_dir(stat.st_mode) {
                if let Some(child_fd) = open_dir_at(raw_fd, &name) {
                    return own + fast_dir_bytes(child_fd);
                }
            }
            own
        })
        .sum();

    unsafe { libc::closedir(dir); }
    total
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

fn mode_is_reg(mode: libc::mode_t) -> bool {
    mode & libc::S_IFMT == libc::S_IFREG
}

fn stat_on_disk_bytes(stat: &libc::stat) -> u64 {
    on_disk_bytes_from_blocks(stat.st_blocks)
}

/// On-disk bytes for one already-stat'd entry, opened relative to `parent_fd`.
///
/// For a regular file this is the clone-aware *private* size — bytes unique to
/// the file, i.e. what deleting it actually frees (issue #47) — whenever the
/// filesystem supplies `ATTR_CMNEXT_PRIVATESIZE`. Otherwise (directories,
/// symlinks, older macOS, non-APFS, or any error) it is the allocated-block size
/// from `stat`, which never reports more than today's behaviour (ADR-0006).
///
/// Summing private sizes across a subtree can under-count when two clones inside
/// the same subtree share blocks (those blocks are private to neither file), but
/// under-counting is the safe direction for a tool that must never overstate what
/// reclaiming frees.
fn entry_on_disk_bytes(parent_fd: RawFd, name: &CStr, stat: &libc::stat) -> u64 {
    if mode_is_reg(stat.st_mode) {
        if let Some(private) = getattrlistat_private_size(parent_fd, name) {
            return private;
        }
    }
    stat_on_disk_bytes(stat)
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

    /// Issue #40: a matched directory's reported size must include its full
    /// subtree, not just the directory node's own allocated blocks.
    /// The walk prunes recursion for matched dirs and uses getattrlist instead.
    #[test]
    fn matched_dir_size_includes_full_subtree_content() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // node_modules with nested real content (non-zero to avoid APFS
        // transparent compression collapsing allocated blocks to near zero).
        let nm = root.join("node_modules");
        let pkg = nm.join("pkg").join("lib");
        fs::create_dir_all(&pkg).unwrap();
        // 8 × 8KB files ≈ 64KB total payload.
        let payload: Vec<u8> = (0u8..=255).cycle().take(8 * 1024).collect();
        for i in 0..8u8 {
            fs::write(pkg.join(format!("m{i}.js")), &payload).unwrap();
        }

        let scan = run(root, &Ruleset::defaults(), 1);
        let nm_item = item_for(&scan, "node_modules");

        assert_eq!(nm_item.class, SafetyClass::Reinstallable);
        // The reported size must exceed a single empty-directory allocation
        // (~4–8 KB); if it were just the dir node's own blocks the optimization
        // broke and the size would be far too small.
        assert!(
            nm_item.size_on_disk >= 8 * 8 * 1024,
            "size {} should cover all nested file content (>= {})",
            nm_item.size_on_disk,
            8 * 8 * 1024,
        );
    }

    /// Issue #43 (headline): a matched directory's size must track its actual
    /// content, not a stray attribute. The earlier code requested `ATTR_CMN_MODTIME`
    /// (commonattr 0x400) thinking it was a total-size attribute, so every matched
    /// dir reported the current Unix timestamp (~1.7 GiB) and the reclaimable total
    /// blew past the physical disk. The old `>= content` lower bound passed straight
    /// through that garbage; this pins an upper bound so the regression can't return.
    #[test]
    fn matched_dir_size_tracks_content_not_a_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let nm = root.join("node_modules");
        fs::create_dir_all(&nm).unwrap();
        // ~256 KB of real payload (incompressible, so blocks are actually allocated).
        let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
        for i in 0..4u8 {
            fs::write(nm.join(format!("m{i}.bin")), &payload).unwrap();
        }

        let scan = run(root, &Ruleset::defaults(), 1);
        let nm_item = item_for(&scan, "node_modules");

        // Real content is ~256 KB; allow generous slack for filesystem overhead but
        // stay far below the ~1.7 GiB a leaked timestamp would produce.
        assert!(
            nm_item.size_on_disk < 64 * 1024 * 1024,
            "size {} looks like a leaked attribute, not 256 KB of content",
            nm_item.size_on_disk,
        );
        assert!(nm_item.size_on_disk >= 4 * 64 * 1024);
    }

    /// Issue #43 (root cause #2): no Reclaimable Item may surface below a matched
    /// Irreplaceable ancestor. OrbStack's Docker data holds overlay layers that
    /// each carry their own node_modules/target; matching `OrbStack/docker` whole
    /// must prune them so they never inflate the reclaimable total.
    #[test]
    fn no_reclaimable_item_below_a_matched_irreplaceable_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // ~/OrbStack/docker — matched Irreplaceable (one opaque block).
        let docker = root.join("OrbStack").join("docker");
        // A snapshot/overlay layer carrying caches that WOULD match on their own.
        let layer = docker.join("containers").join("layer0");
        let inner_nm = layer.join("node_modules");
        let inner_target = layer.join("app").join("target");
        fs::create_dir_all(&inner_nm).unwrap();
        fs::create_dir_all(&inner_target).unwrap();
        fs::write(layer.join("app").join("Cargo.toml"), b"[package]\n").unwrap();
        let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
        fs::write(inner_nm.join("index.js"), &payload).unwrap();
        fs::write(inner_target.join("build-output"), &payload).unwrap();

        let scan = run(root, &Ruleset::defaults(), 1);

        let docker_item = item_for(&scan, "docker");
        assert_eq!(docker_item.class, SafetyClass::Irreplaceable);
        assert!(docker_item.class.is_protected());
        assert!(!docker_item.may_reclaim());

        // Nothing inside the matched Irreplaceable subtree may appear as its own
        // Item — not the overlay's node_modules, not its target/.
        for inner in ["node_modules", "target", "layer0", "containers"] {
            assert!(
                scan.items
                    .iter()
                    .all(|i| i.path.file_name().and_then(|n| n.to_str()) != Some(inner)),
                "{inner} below OrbStack/docker must be pruned, not surfaced",
            );
        }

        // The reclaimable total counts none of the pruned inner caches.
        assert_eq!(scan.reclaimable_bytes(), 0);
    }

    /// Issue #43 (root cause #3): Treehouse's per-PR workspaces collapse into a
    /// single `.treehouse` Item instead of thousands of inner node_modules/target
    /// Items. The whole tree is the Reclaimable unit.
    #[test]
    fn treehouse_collapses_into_one_reclaimable_item() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // ~/.treehouse/<project>/<n> isolated checkouts, each with its own caches.
        let payload: Vec<u8> = (0u8..=255).cycle().take(64 * 1024).collect();
        for n in 0..3 {
            let env = root.join(".treehouse").join("proj").join(n.to_string());
            let nm = env.join("node_modules");
            let target = env.join("target");
            fs::create_dir_all(&nm).unwrap();
            fs::create_dir_all(&target).unwrap();
            fs::write(env.join("Cargo.toml"), b"[package]\n").unwrap();
            fs::write(nm.join("index.js"), &payload).unwrap();
            fs::write(target.join("build-output"), &payload).unwrap();
        }

        let scan = run(root, &Ruleset::defaults(), 1);

        let treehouse = item_for(&scan, ".treehouse");
        assert_eq!(treehouse.class, SafetyClass::Regenerable);
        assert!(treehouse.may_reclaim());
        // Inner workspaces are subsumed, never surfaced on their own.
        assert!(
            scan.items
                .iter()
                .all(|i| i.path.file_name().and_then(|n| n.to_str()) != Some("node_modules")),
            "per-workspace node_modules must be pruned under .treehouse",
        );
        // The consolidated Item is the only Reclaimable, and it carries the bytes.
        assert_eq!(scan.reclaimable_bytes(), treehouse.size_on_disk);
        assert!(treehouse.size_on_disk >= 6 * 64 * 1024);
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

    /// Acceptance for issue #47: a subtree of APFS clones reports the *private*
    /// (freeable) bytes, not the full logical allocation. The clones share blocks
    /// via copy-on-write, so deleting the subtree frees roughly one copy's worth —
    /// and because all references live inside the subtree, the safe under-count
    /// (ADR-0006 / issue #47) drives the reported size well below even one copy.
    ///
    /// Skips gracefully on filesystems without `clonefile(2)`/`PRIVATESIZE` (e.g.
    /// a non-APFS tmpfs), since macOS defaults to APFS where this runs for real.
    /// The always-on assertion below covers the sizing path unconditionally.
    #[test]
    fn cloned_subtree_reports_private_not_logical_bytes() {
        use std::os::unix::ffi::OsStrExt;

        const N: usize = 8 * 1024 * 1024; // well above block + dir overhead
        const CLONES: usize = 4;

        let blob = vec![0xABu8; N];

        // --- Always-on coverage: a plain (non-cloned) file reports its full
        // allocation, proving the private-size path doesn't under-count the
        // ordinary case. Runs on every filesystem. ---
        {
            let tmp = tempfile::tempdir().unwrap();
            let plain = tmp.path().join("plain.bin");
            fs::write(&plain, &blob).unwrap();
            let measured = fast_tree_bytes(&plain);
            assert!(
                measured as usize >= N && (measured as usize) < N + 1024 * 1024,
                "non-cloned file should report ~its allocation: {measured} vs {N}"
            );
        }

        // --- Clone case (APFS only). ---
        extern "C" {
            fn clonefile(
                src: *const libc::c_char,
                dst: *const libc::c_char,
                flags: u32,
            ) -> libc::c_int;
        }
        fn cstr(p: &Path) -> CString {
            CString::new(p.as_os_str().as_bytes()).unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let subtree = tmp.path().join("clones");
        fs::create_dir(&subtree).unwrap();
        let source = subtree.join("source.bin");
        fs::write(&source, &blob).unwrap();

        // Skip if this filesystem can't supply per-file private size at all.
        if getattrlistat_private_size(libc::AT_FDCWD, &cstr(&source)).is_none() {
            eprintln!("skipping: ATTR_CMNEXT_PRIVATESIZE unavailable on this fs");
            return;
        }

        let src_c = cstr(&source);
        for i in 0..CLONES {
            let dst = subtree.join(format!("clone_{i}.bin"));
            let dst_c = cstr(&dst);
            let rc = unsafe { clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::ENOTSUP) {
                    eprintln!("skipping: clonefile unsupported on this fs ({err})");
                    return;
                }
                panic!("clonefile failed: {err}");
            }
        }

        let logical = (CLONES + 1) * N; // what naive allocated-block sizing reports
        let measured = fast_tree_bytes(&subtree) as usize;

        // The clones share one physical copy; every block is referenced by more
        // than one file, so each file's private size is ~0 and the subtree sizes
        // far below even a single logical copy — and nowhere near the full
        // logical allocation it would have reported before issue #47.
        assert!(
            measured < N,
            "cloned subtree should report private bytes (<{N}), got {measured} \
             (full logical would be {logical})"
        );
    }
}
