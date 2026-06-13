//! Update check (ADR-0009): ask GitHub for the latest published Release and tell
//! the user whether a newer macclean exists. Shells out to `curl` — no new crate,
//! consistent with ADR-0007's curl-based install — and fails soft so a missing
//! curl, a down network, or an unparseable response never panics and never hangs
//! (bounded `--max-time`). The fetch + compare live here as a reusable function,
//! not buried in the CLI handler, so the TUI "update available" banner slice
//! (issue #32) can call the same brain.

use std::process::Command;

/// The advertised upgrade command (ADR-0007). Printed verbatim when a newer
/// release exists so the user can reinstall in one line.
pub const INSTALL_ONE_LINER: &str = "curl -fsSL https://ez4t.github.io/macclean/install.sh | sh";

/// GitHub's `releases/latest` redirect for this repo. Resolving it with curl
/// yields the canonical `.../releases/tag/vX.Y.Z` URL we read the tag from — no
/// GitHub API call, so no rate limits or User-Agent requirement.
const LATEST_RELEASE_URL: &str = "https://github.com/Ez4T/macclean/releases/latest";

/// Hard ceiling on the network request so the check can never hang (issue #31).
const FETCH_TIMEOUT_SECS: u32 = 5;

/// A plain `MAJOR.MINOR.PATCH` version. Field-declaration order makes the derived
/// `Ord` compare major, then minor, then patch — exactly the "is the released tag
/// newer than ours?" ordering, so no semver crate is needed (issue #31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// The running build's version, from `CARGO_PKG_VERSION` (never hard-coded).
    pub fn current() -> Version {
        parse_tag(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is valid MAJOR.MINOR.PATCH")
    }

    /// Render as a `vX.Y.Z` tag for display.
    pub fn tag(&self) -> String {
        format!("v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parse a `vMAJOR.MINOR.PATCH` (or bare `MAJOR.MINOR.PATCH`) tag. Returns `None`
/// for anything malformed — an extra dotted segment, a non-numeric component, a
/// prerelease suffix — so every caller fails soft rather than panicking.
pub fn parse_tag(tag: &str) -> Option<Version> {
    let core = tag.trim().trim_start_matches('v');
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// Pull the tag out of a resolved `releases/latest` URL, e.g.
/// `https://github.com/Ez4T/macclean/releases/tag/v0.2.0` → `v0.2.0`. Returns
/// `None` if the URL isn't a `/tag/<name>` form so the caller fails soft.
fn tag_from_release_url(url: &str) -> Option<&str> {
    let (_, tag) = url.trim_end_matches('/').rsplit_once("/tag/")?;
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(tag)
}

/// Resolve the latest-release redirect with curl and return its parsed tag. Any
/// failure (curl missing, network down, HTTP error, unparseable redirect or tag)
/// is an `Err(reason)` the caller turns into a fail-soft "couldn't check".
fn fetch_latest_version() -> Result<Version, String> {
    let timeout = FETCH_TIMEOUT_SECS.to_string();
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            &timeout,
            "-o",
            "/dev/null",
            // Print the post-redirect URL (…/releases/tag/vX.Y.Z) instead of body.
            "-w",
            "%{url_effective}",
            LATEST_RELEASE_URL,
        ])
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if !output.status.success() {
        return Err("network request failed".to_string());
    }
    let url = String::from_utf8_lossy(&output.stdout);
    let tag = tag_from_release_url(url.trim()).ok_or("could not read the release tag")?;
    parse_tag(tag).ok_or_else(|| format!("unrecognized release tag: {tag}"))
}

/// The outcome of an update check. `Failed` carries a short reason so the CLI and
/// the TUI banner can both present a fail-soft note without re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    UpToDate {
        current: Version,
    },
    Newer {
        current: Version,
        latest: Version,
    },
    Failed {
        reason: String,
    },
}

/// The reusable update-check brain (issue #31): fetch the latest released tag and
/// compare it to the running build. Fails soft — never panics, never hangs. Both
/// the `--check-update` CLI handler and the TUI banner slice call this.
pub fn check_for_update() -> UpdateCheck {
    let current = Version::current();
    match fetch_latest_version() {
        Ok(latest) if latest > current => UpdateCheck::Newer { current, latest },
        Ok(_) => UpdateCheck::UpToDate { current },
        Err(reason) => UpdateCheck::Failed { reason },
    }
}

/// CLI presenter for `macclean --check-update`: run the check and print a human
/// line. Always returns success — a failed check is informational, not an error.
pub fn print_check_update() {
    match check_for_update() {
        UpdateCheck::UpToDate { current } => {
            println!("macclean {} is up to date.", current.tag());
        }
        UpdateCheck::Newer { latest, .. } => {
            println!(
                "A newer version {} is available — reinstall:\n  {}",
                latest.tag(),
                INSTALL_ONE_LINER
            );
        }
        UpdateCheck::Failed { reason } => {
            println!("Couldn't check for updates ({reason}).");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v_prefixed_and_bare_tags() {
        assert_eq!(
            parse_tag("v1.2.3"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(
            parse_tag("0.10.0"),
            Some(Version {
                major: 0,
                minor: 10,
                patch: 0
            })
        );
        // Surrounding whitespace (e.g. a trailing newline from curl) is tolerated.
        assert_eq!(
            parse_tag(" v2.0.1 \n"),
            Some(Version {
                major: 2,
                minor: 0,
                patch: 1
            })
        );
    }

    #[test]
    fn rejects_malformed_tags() {
        for bad in ["", "v1", "1.2", "1.2.3.4", "v1.2.x", "vfoo", "1.2.3-rc1", "latest"] {
            assert_eq!(parse_tag(bad), None, "should reject {bad:?}");
        }
    }

    /// The compare is plain tuple ordering: older < equal < newer, across each
    /// component (issue #31).
    #[test]
    fn version_ordering_is_major_minor_patch() {
        let v = |a, b, c| Version {
            major: a,
            minor: b,
            patch: c,
        };
        // Older than current.
        assert!(v(0, 9, 9) < v(1, 0, 0));
        assert!(v(1, 0, 0) < v(1, 0, 1));
        assert!(v(1, 1, 0) < v(1, 2, 0));
        // Equal.
        assert_eq!(v(1, 2, 3), v(1, 2, 3));
        assert!(!(v(1, 2, 3) > v(1, 2, 3)));
        // Newer wins regardless of lower components.
        assert!(v(2, 0, 0) > v(1, 9, 9));
        assert!(v(1, 3, 0) > v(1, 2, 9));
    }

    #[test]
    fn extracts_tag_from_release_redirect_url() {
        assert_eq!(
            tag_from_release_url("https://github.com/Ez4T/macclean/releases/tag/v0.2.0"),
            Some("v0.2.0")
        );
        // A trailing slash is tolerated.
        assert_eq!(
            tag_from_release_url("https://github.com/Ez4T/macclean/releases/tag/v1.0.0/"),
            Some("v1.0.0")
        );
        // No `/tag/` segment (e.g. the un-redirected latest URL) → None.
        assert_eq!(
            tag_from_release_url("https://github.com/Ez4T/macclean/releases/latest"),
            None
        );
        assert_eq!(tag_from_release_url("not a url"), None);
    }

    /// `current()` reads the compiled-in version and round-trips through `tag()`.
    #[test]
    fn current_matches_cargo_pkg_version() {
        let current = Version::current();
        assert_eq!(current, parse_tag(env!("CARGO_PKG_VERSION")).unwrap());
        assert_eq!(current.tag(), format!("v{}", env!("CARGO_PKG_VERSION")));
    }
}
