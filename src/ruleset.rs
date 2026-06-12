//! The data-driven classification ruleset (ADR-0003). Rules are data, not code:
//! the curated defaults below are bundled, and users may add or override Rules
//! via a config file. An Item matching no Rule becomes `Unclassified` (ADR-0001).

use crate::model::SafetyClass;
use serde::{Deserialize, Serialize};

/// What a [`Rule`] looks for on disk (CONTEXT.md → "Rule", the match condition).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Match {
    /// A directory whose name equals `dir`, sitting beside a file named
    /// `sibling` (e.g. `target/` next to `Cargo.toml`).
    DirBesideSibling { dir: String, sibling: String },
    /// A directory whose absolute path matches this glob-ish suffix
    /// (e.g. `*/.cache/uv`). Used for well-known cache locations.
    PathSuffix { suffix: String },
    /// A directory whose name equals `dir`, regardless of siblings
    /// (e.g. `node_modules`).
    DirNamed { dir: String },
    /// An Item (file or directory) whose own name ends with `suffix`
    /// (e.g. `.qcow2`, `.img.raw`). Used to recognize files by extension,
    /// such as VM disk images.
    NameSuffix { suffix: String },
    /// A directory that directly contains a child file named `file`
    /// (e.g. a `PG_VERSION` marker identifying a PostgreSQL data directory).
    /// The contained file is the Evidence; the matched Item is the directory.
    DirContainingFile { file: String },
}

/// A single declarative classification entry (CONTEXT.md → "Rule"). Maps a
/// [`Match`] to a [`SafetyClass`] and, for Regenerable/Reinstallable, the clean
/// command that defines its Recovery Method (ADR-0002).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    pub matches: Match,
    pub class: SafetyClass,
    /// Canonical clean command, run in the Item's parent dir if present
    /// (ADR-0002). `None` means reclaim falls back to a direct `rm`.
    #[serde(default)]
    pub clean_command: Option<String>,
    /// Command that recreates the Item, shown as the Recovery Method.
    #[serde(default)]
    pub recover_command: Option<String>,
    /// Sentence shown as Evidence when this Rule fires.
    pub evidence: String,
}

/// The collection of Rules the classifier applies (CONTEXT.md → "Ruleset").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ruleset {
    pub rules: Vec<Rule>,
}

impl Ruleset {
    /// The curated default Ruleset shipped with the app. Mirrors the classes we
    /// hand-applied in the originating cleanup session.
    pub fn defaults() -> Self {
        let r = |name: &str,
                 matches: Match,
                 class: SafetyClass,
                 clean: Option<&str>,
                 recover: Option<&str>,
                 evidence: &str| Rule {
            name: name.into(),
            matches,
            class,
            clean_command: clean.map(Into::into),
            recover_command: recover.map(Into::into),
            evidence: evidence.into(),
        };

        Ruleset {
            rules: vec![
                r(
                    "rust-target",
                    Match::DirBesideSibling { dir: "target".into(), sibling: "Cargo.toml".into() },
                    SafetyClass::Regenerable,
                    Some("cargo clean"),
                    Some("cargo build"),
                    "Cargo.toml sits beside this target/ — Rust build output.",
                ),
                r(
                    "flutter-build",
                    Match::DirBesideSibling { dir: "build".into(), sibling: "pubspec.yaml".into() },
                    SafetyClass::Regenerable,
                    Some("flutter clean"),
                    Some("flutter pub get && flutter build"),
                    "pubspec.yaml sits beside this build/ — Flutter build output.",
                ),
                r(
                    "next-build",
                    Match::DirBesideSibling { dir: ".next".into(), sibling: "package.json".into() },
                    SafetyClass::Regenerable,
                    None,
                    Some("next build"),
                    "package.json sits beside this .next/ — Next.js build output.",
                ),
                r(
                    "node-modules",
                    Match::DirNamed { dir: "node_modules".into() },
                    SafetyClass::Reinstallable,
                    None,
                    Some("npm install"),
                    "node_modules — restorable by the package manager.",
                ),
                r(
                    "npm-cache",
                    Match::PathSuffix { suffix: ".npm".into() },
                    SafetyClass::Cache,
                    None,
                    None,
                    "npm download cache — refilled automatically on next install.",
                ),
                r(
                    "uv-cache",
                    Match::PathSuffix { suffix: ".cache/uv".into() },
                    SafetyClass::Cache,
                    None,
                    None,
                    "uv cache — refilled automatically on next use.",
                ),
                // Irreplaceable data → Protected (CONTEXT.md, ADR-0001). These make
                // real, non-recoverable data recognized as data rather than relying
                // on the Unclassified backstop. None carry a Recovery Method; none
                // are ever offered for deletion.
                r(
                    "vm-disk-image-raw",
                    Match::NameSuffix { suffix: ".img.raw".into() },
                    SafetyClass::Irreplaceable,
                    None,
                    None,
                    "A raw VM/container disk image (.img.raw) — live volume, not regenerable from any command.",
                ),
                r(
                    "vm-disk-image-qcow2",
                    Match::NameSuffix { suffix: ".qcow2".into() },
                    SafetyClass::Irreplaceable,
                    None,
                    None,
                    "A QEMU/qcow2 VM disk image — live volume, not regenerable from any command.",
                ),
                r(
                    "vm-disk-image-vdi",
                    Match::NameSuffix { suffix: ".vdi".into() },
                    SafetyClass::Irreplaceable,
                    None,
                    None,
                    "A VirtualBox VDI disk image — live volume, not regenerable from any command.",
                ),
                r(
                    "vm-disk-image-vmdk",
                    Match::NameSuffix { suffix: ".vmdk".into() },
                    SafetyClass::Irreplaceable,
                    None,
                    None,
                    "A VMware VMDK disk image — live volume, not regenerable from any command.",
                ),
                r(
                    "postgres-data-dir",
                    Match::DirContainingFile { file: "PG_VERSION".into() },
                    SafetyClass::Irreplaceable,
                    None,
                    None,
                    "A PG_VERSION marker sits here — this is a live PostgreSQL data directory.",
                ),
            ],
        }
    }

    /// Load user Rules from TOML and append them after the defaults so user
    /// entries override on first-match. Returns the merged Ruleset.
    pub fn with_user_rules(mut self, toml_text: &str) -> anyhow::Result<Self> {
        let user: Ruleset = toml::from_str(toml_text)?;
        // User rules take precedence: evaluate them first.
        let mut merged = user.rules;
        merged.append(&mut self.rules);
        self.rules = merged;
        Ok(self)
    }
}
