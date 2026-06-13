//! Filesystem measurement. Sizes are actual on-disk allocated bytes, not
//! apparent file length (ADR-0006), so sparse images and clones are not
//! overstated.

/// Convert allocated 512-byte block counts into actual on-disk bytes
/// (ADR-0006), not apparent file length.
pub fn on_disk_bytes_from_blocks(blocks_512: i64) -> u64 {
    u64::try_from(blocks_512).unwrap_or(0).saturating_mul(512)
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
