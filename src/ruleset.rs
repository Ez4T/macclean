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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::match_rule_typed;
    use std::fs;
    use std::path::Path;

    // Match-condition coverage (issue #6). Each `Match` variant is exercised
    // through the public matcher so the data-driven Ruleset (ADR-0003) is proven
    // end-to-end, not just deserialized.

    #[test]
    fn dir_named_matches_only_directories_by_name() {
        let rs = Ruleset::defaults();
        // node_modules as a directory matches the Reinstallable Rule…
        let rule = match_rule_typed(&rs, Path::new("/x/node_modules"), true)
            .expect("node_modules dir should match");
        assert_eq!(rule.name, "node-modules");
        assert_eq!(rule.class, SafetyClass::Reinstallable);
        // …but a *file* named node_modules does not (DirNamed is dir-only).
        assert!(match_rule_typed(&rs, Path::new("/x/node_modules"), false).is_none());
    }

    #[test]
    fn dir_beside_sibling_requires_the_sibling_on_disk() {
        let rs = Ruleset::defaults();
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let target = proj.join("target");
        fs::create_dir_all(&target).unwrap();

        // Without the Cargo.toml sibling, the target/ dir is unrecognized.
        assert!(
            match_rule_typed(&rs, &target, true).is_none(),
            "target/ alone must not match — the sibling is the Evidence",
        );

        // With Cargo.toml beside it, the Rust build-output Rule fires.
        fs::write(proj.join("Cargo.toml"), b"[package]\n").unwrap();
        let rule = match_rule_typed(&rs, &target, true).expect("target beside Cargo.toml matches");
        assert_eq!(rule.name, "rust-target");
        assert_eq!(rule.class, SafetyClass::Regenerable);
        assert_eq!(rule.clean_command.as_deref(), Some("cargo clean"));
    }

    #[test]
    fn path_suffix_matches_well_known_cache_locations() {
        let rs = Ruleset::defaults();
        let rule = match_rule_typed(&rs, Path::new("/home/me/.cache/uv"), true)
            .expect(".cache/uv should match the uv cache Rule");
        assert_eq!(rule.name, "uv-cache");
        assert_eq!(rule.class, SafetyClass::Cache);
        // A path that merely contains the fragment mid-string does not end with it.
        assert!(match_rule_typed(&rs, Path::new("/home/.cache/uv/blobs"), true).is_none());
    }

    #[test]
    fn name_suffix_recognizes_vm_images_as_irreplaceable() {
        let rs = Ruleset::defaults();
        let rule = match_rule_typed(&rs, Path::new("/vm/disk.qcow2"), false)
            .expect(".qcow2 should match");
        assert_eq!(rule.class, SafetyClass::Irreplaceable);
        assert!(rule.clean_command.is_none(), "Irreplaceable carries no clean command");
    }

    #[test]
    fn dir_containing_file_recognizes_a_postgres_data_dir() {
        let rs = Ruleset::defaults();
        let tmp = tempfile::tempdir().unwrap();
        let pg = tmp.path().join("pgdata");
        fs::create_dir(&pg).unwrap();

        assert!(match_rule_typed(&rs, &pg, true).is_none(), "no PG_VERSION marker yet");
        fs::write(pg.join("PG_VERSION"), b"16\n").unwrap();
        let rule = match_rule_typed(&rs, &pg, true).expect("PG_VERSION marker matches");
        assert_eq!(rule.class, SafetyClass::Irreplaceable);
    }

    /// User-rule precedence (ADR-0003): a user Rule is evaluated before the
    /// bundled defaults, so it can reclassify a path the defaults already cover.
    #[test]
    fn user_rules_take_precedence_over_defaults() {
        // Default ruleset classifies node_modules as Reinstallable.
        let defaults = Ruleset::defaults();
        assert_eq!(
            match_rule_typed(&defaults, Path::new("/x/node_modules"), true)
                .unwrap()
                .class,
            SafetyClass::Reinstallable,
        );

        // A user rule re-labels node_modules as Cache; it must win on first-match.
        let user_toml = r#"
            [[rules]]
            name = "treat-node-modules-as-cache"
            class = "Cache"
            evidence = "user override"

            [rules.matches]
            DirNamed = { dir = "node_modules" }
        "#;
        let merged = Ruleset::defaults().with_user_rules(user_toml).unwrap();
        let rule = match_rule_typed(&merged, Path::new("/x/node_modules"), true).unwrap();
        assert_eq!(rule.name, "treat-node-modules-as-cache");
        assert_eq!(rule.class, SafetyClass::Cache, "user rule overrides the default");
    }

    #[test]
    fn malformed_user_toml_is_an_error_not_a_panic() {
        assert!(Ruleset::defaults().with_user_rules("this is = not valid").is_err());
    }
}
