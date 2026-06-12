//! Post-classification Redundant Copy detection (CONTEXT.md → "Redundant Copy").
//!
//! A [`SafetyClass::RedundantCopy`] is a byte-identical duplicate of data that
//! survives elsewhere: deleting it loses nothing. Nothing in classification
//! produces it — duplication is a relationship *between* Items, not a property of
//! one — so this runs as a second pass over a finished [`Scan`].
//!
//! Cost control (per issue scope): on-disk size is the cheap pre-filter — only
//! Items of exactly equal size can be byte-identical — and `blake3` confirms
//! equality, so we never hash an Item that has no size-peer.

use crate::model::{Evidence, Item, RecoveryMethod, SafetyClass, Scan};
use jwalk::WalkDir;
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// Detect byte-identical Items in `scan` and relabel all but one of each
/// duplicate group as [`SafetyClass::RedundantCopy`], pointing at the surviving
/// original via [`RecoveryMethod::SurvivingCopy`]. The survivor keeps its own
/// class — including `Irreplaceable`, so a Protected original is never the Item
/// we offer to delete.
pub fn analyze(scan: &mut Scan) {
    // Cheap pre-filter: bucket Items by on-disk size; only same-size Items can be
    // byte-identical, so anything alone in its bucket is never hashed.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, item) in scan.items.iter().enumerate() {
        if item.size_on_disk == 0 {
            continue; // empty Items would all collide; nothing to reclaim anyway.
        }
        by_size.entry(item.size_on_disk).or_default().push(i);
    }

    for indices in by_size.into_values() {
        if indices.len() < 2 {
            continue;
        }

        // Confirm equality with blake3: group the size-peers by content hash.
        let mut by_hash: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
        for i in indices {
            if let Some(hash) = content_fingerprint(&scan.items[i].path) {
                by_hash.entry(*hash.as_bytes()).or_default().push(i);
            }
        }

        for mut group in by_hash.into_values() {
            if group.len() < 2 {
                continue;
            }
            mark_redundant(&mut scan.items, &mut group);
        }
    }
}

/// Choose the survivor of a byte-identical `group` and relabel the rest. The
/// survivor is the most-worth-keeping copy: a Protected (`Irreplaceable`) Item
/// first so the guardrail is never the Item we make reclaimable, then the
/// shortest path (a `-backup`/`copy` sibling is longer than its original), then
/// lexicographic order for a fully deterministic result.
fn mark_redundant(items: &mut [Item], group: &mut [usize]) {
    group.sort_by(|&a, &b| {
        let (ia, ib) = (&items[a], &items[b]);
        ib.class
            .is_protected()
            .cmp(&ia.class.is_protected())
            .then_with(|| ia.path.as_os_str().len().cmp(&ib.path.as_os_str().len()))
            .then_with(|| ia.path.as_os_str().cmp(ib.path.as_os_str()))
    });

    let original = items[group[0]].path.clone();
    for &dup in &group[1..] {
        let item = &mut items[dup];
        item.class = SafetyClass::RedundantCopy;
        item.recovery = RecoveryMethod::SurvivingCopy { original: original.clone() };
        item.evidence = Evidence {
            summary: format!("byte-identical to {}", original.display()),
        };
        // A relabel cannot inherit a stale Unclassified override.
        item.override_reclaim = false;
    }
}

/// A content hash that is identical for byte-identical Items regardless of their
/// own name or where they sit. For a file it is the hash of its bytes; for a
/// directory it is the hash of its sorted `(relative path, file hash)` listing,
/// so structure and contents both count but the tree's own name does not.
fn content_fingerprint(path: &Path) -> Option<blake3::Hash> {
    let mut lines: Vec<String> = Vec::new();
    for entry in WalkDir::new(path)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let p = entry.path();
        let rel = p.strip_prefix(path).unwrap_or(&p).to_string_lossy();
        let ft = entry.file_type();
        if ft.is_dir() {
            lines.push(format!("D:{rel}"));
        } else if ft.is_file() {
            let h = hash_file(&p)?;
            lines.push(format!("F:{rel}:{}", h.to_hex()));
        }
        // Symlinks and other special files are not compared.
    }
    lines.sort();

    let mut hasher = blake3::Hasher::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    Some(hasher.finalize())
}

/// Stream a file through blake3 without loading it whole into memory.
fn hash_file(path: &Path) -> Option<blake3::Hash> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher).ok()?;
    Some(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reclaim::{self, Reclaimed};
    use crate::ruleset::Ruleset;
    use crate::{classify, scan};
    use std::fs;

    fn item_for<'a>(scan: &'a Scan, name: &str) -> &'a Item {
        scan.items
            .iter()
            .find(|i| i.path.file_name().and_then(|n| n.to_str()) == Some(name))
            .unwrap_or_else(|| panic!("no Item named {name} in scan"))
    }

    /// Acceptance for issue #2: two identical directories surface with one as
    /// RedundantCopy (evidence pointing at the other) and the original untouched;
    /// reclaiming the copy permanently deletes it and frees space (ADR-0004).
    #[test]
    fn identical_dirs_yield_one_redundant_copy_then_reclaim_deletes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Two byte-identical trees; the `-backup` name is the longer path, so the
        // bare `alpha` is chosen as the surviving original.
        for dir in ["alpha", "alpha-backup"] {
            let d = root.join(dir);
            fs::create_dir_all(d.join("nested")).unwrap();
            fs::write(d.join("a.bin"), vec![7u8; 8192]).unwrap();
            fs::write(d.join("nested").join("b.bin"), vec![9u8; 8192]).unwrap();
        }

        // Low threshold so both unmatched dirs surface as Items to compare.
        let mut s = classify::run(root, &Ruleset::defaults(), 1);
        analyze(&mut s);

        let original = item_for(&s, "alpha");
        let copy = item_for(&s, "alpha-backup");

        assert_eq!(copy.class, SafetyClass::RedundantCopy);
        assert!(copy.may_reclaim());
        assert_eq!(
            copy.recovery,
            RecoveryMethod::SurvivingCopy { original: original.path.clone() }
        );
        assert!(copy.evidence.summary.contains("byte-identical to"));
        // The original is left alone — still just an Unclassified unknown.
        assert_eq!(original.class, SafetyClass::Unclassified);

        // Reclaiming the copy removes it permanently (no Trash for Reclaimables).
        let copy_path = copy.path.clone();
        let copy = copy.clone();
        let original_path = original.path.clone();
        match reclaim::reclaim(&copy, &Ruleset::defaults()).unwrap() {
            Reclaimed::Removed => {}
            other => panic!("expected permanent removal, got {other:?}"),
        }
        assert!(!copy_path.exists(), "Redundant Copy should be gone");
        assert!(original_path.exists(), "the surviving original must remain");
    }

    /// A Redundant Copy can be a *file*, and when both copies are Protected the
    /// survivor must stay Irreplaceable — we never make the guardrail the Item we
    /// offer to delete.
    #[test]
    fn duplicate_protected_files_keep_one_protected_survivor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Two identical VM images: both classify Irreplaceable (issue #1 rules).
        fs::write(root.join("disk1.img.raw"), vec![3u8; 16384]).unwrap();
        fs::write(root.join("disk2.img.raw"), vec![3u8; 16384]).unwrap();

        let mut s = classify::run(root, &Ruleset::defaults(), 1);
        analyze(&mut s);

        // Deterministic: equal protection and equal path length → lexicographic,
        // so disk1 survives as the Protected original.
        let survivor = item_for(&s, "disk1.img.raw");
        let copy = item_for(&s, "disk2.img.raw");
        assert_eq!(survivor.class, SafetyClass::Irreplaceable);
        assert!(survivor.class.is_protected());
        assert!(!survivor.may_reclaim());
        assert_eq!(copy.class, SafetyClass::RedundantCopy);
        assert!(copy.may_reclaim());

        // Reclaiming the file copy uses the file-removal path and frees space.
        let copy_path = copy.path.clone();
        let freed = copy.size_on_disk;
        let copy = copy.clone();
        match reclaim::reclaim(&copy, &Ruleset::defaults()).unwrap() {
            Reclaimed::Removed => {}
            other => panic!("expected file removal, got {other:?}"),
        }
        assert!(!copy_path.exists());
        assert!(survivor.path.exists());
        assert!(freed > 0, "on-disk size should be measured (was {})", scan::human(freed));
    }
}
