//! Filesystem measurement. Sizes are actual on-disk allocated bytes, not
//! apparent file length (ADR-0006), so sparse images and clones are not
//! overstated.

use jwalk::WalkDir;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Actual on-disk bytes for a single path's own metadata: allocated 512-byte
/// blocks (ADR-0006), not `len()`.
pub fn entry_on_disk_bytes(meta: &std::fs::Metadata) -> u64 {
    // `blocks()` counts 512-byte units actually allocated; sparse holes and
    // unwritten regions are excluded.
    meta.blocks() * 512
}

/// Recursively sum actual on-disk bytes under `path` (the path itself plus all
/// descendants), walked in parallel.
pub fn on_disk_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|e| e.metadata().ok())
        .map(|m| entry_on_disk_bytes(&m))
        .sum()
}

/// Human-friendly size, matching the `du -h` style used throughout the project.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{}{}", bytes, UNITS[0])
    } else {
        format!("{:.1}{}", v, UNITS[u])
    }
}
