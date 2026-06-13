//! Filesystem measurement. Sizes are actual on-disk allocated bytes, not
//! apparent file length (ADR-0006), so sparse images and clones are not
//! overstated.

use std::os::unix::fs::MetadataExt;

/// Actual on-disk bytes for a single path's own metadata: allocated 512-byte
/// blocks (ADR-0006), not `len()`.
///
/// Whole-subtree totals are no longer summed here: [`crate::classify`] walks the
/// tree once in parallel and folds these per-entry blocks itself, so a separate
/// recursive re-walk per matched directory is gone.
pub fn entry_on_disk_bytes(meta: &std::fs::Metadata) -> u64 {
    // `blocks()` counts 512-byte units actually allocated; sparse holes and
    // unwritten regions are excluded.
    meta.blocks() * 512
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
