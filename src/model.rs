//! Core domain types. These mirror the glossary in CONTEXT.md one-to-one;
//! keep the names in sync with it.

use std::path::PathBuf;

/// The label assigned to each [`Item`] describing what it is and the cost of
/// deleting it. Exactly six values (CONTEXT.md → "Safety Class").
///
/// The first four are [`SafetyClass::is_reclaimable`]; `Irreplaceable` is
/// Protected; `Unclassified` is surfaced but never auto-offered (ADR-0001).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SafetyClass {
    /// Build output a build command recreates. Recovered by *rebuilding*.
    Regenerable,
    /// Dependencies a package manager restores. Recovered by *reinstalling*.
    Reinstallable,
    /// Data automatically refilled on next use. Recovered by *doing nothing*.
    Cache,
    /// A byte-identical duplicate; the original survives. Recovered by the
    /// *surviving copy*.
    RedundantCopy,
    /// Real, non-recoverable data. Protected: never offered for deletion.
    Irreplaceable,
    /// A large Item no [`crate::ruleset::Rule`] matched. Surfaced, but not
    /// reclaimable until the user explicitly overrides (ADR-0001).
    Unclassified,
}

impl SafetyClass {
    /// Whether the app may *offer* this Item for deletion. The four build/dep/
    /// cache/duplicate classes are reclaimable; Irreplaceable never is, and
    /// Unclassified only after a manual override (CONTEXT.md → "Reclaimable").
    pub fn is_reclaimable(self) -> bool {
        matches!(
            self,
            SafetyClass::Regenerable
                | SafetyClass::Reinstallable
                | SafetyClass::Cache
                | SafetyClass::RedundantCopy
        )
    }

    /// The guardrail state of an Irreplaceable Item (CONTEXT.md → "Protected").
    pub fn is_protected(self) -> bool {
        matches!(self, SafetyClass::Irreplaceable)
    }

    pub fn label(self) -> &'static str {
        match self {
            SafetyClass::Regenerable => "Regenerable",
            SafetyClass::Reinstallable => "Reinstallable",
            SafetyClass::Cache => "Cache",
            SafetyClass::RedundantCopy => "Redundant Copy",
            SafetyClass::Irreplaceable => "Irreplaceable",
            SafetyClass::Unclassified => "Unclassified",
        }
    }
}

/// How a deleted Reclaimable Item is restored (CONTEXT.md → "Recovery Method").
/// Each Reclaimable Safety Class implies exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryMethod {
    /// Run a build tool, e.g. `cargo build`.
    Rebuild { command: String },
    /// Run a package manager, e.g. `npm install`.
    Reinstall { command: String },
    /// Nothing — the tool refills the cache on next use.
    AutoRefill,
    /// Nothing — another copy survives at this path.
    SurvivingCopy { original: PathBuf },
    /// Not recoverable (Irreplaceable) or unknown (Unclassified).
    None,
}

impl RecoveryMethod {
    /// A short human sentence describing how a reclaimed Item is restored, shown
    /// in the TUI detail line and the Confirm prompt (CONTEXT.md → "Recovery
    /// Method"). Pure to the method; for the Trash safety-net of an overridden
    /// Unclassified Item see [`Item::recovery_line`].
    pub fn describe(&self) -> String {
        match self {
            RecoveryMethod::Rebuild { command } => format!("rebuild via `{command}`"),
            RecoveryMethod::Reinstall { command } => format!("reinstall via `{command}`"),
            RecoveryMethod::AutoRefill => "refills automatically on next use".into(),
            RecoveryMethod::SurvivingCopy { original } => {
                format!("the surviving copy at {}", original.display())
            }
            RecoveryMethod::None => "not recoverable".into(),
        }
    }
}

/// The concrete proof the Scan used to assign a Safety Class (CONTEXT.md →
/// "Evidence"). Always shown alongside the class so the user can trust or
/// override it.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// Short human sentence, e.g. "Cargo.toml sits beside this target/ dir".
    pub summary: String,
}

/// A file or directory the Scan surfaces as a single actionable unit
/// (CONTEXT.md → "Item").
#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    /// Actual on-disk size in bytes (allocated blocks), not apparent length
    /// (ADR-0006).
    pub size_on_disk: u64,
    pub class: SafetyClass,
    pub recovery: RecoveryMethod,
    pub evidence: Evidence,
    /// Set true only after the user manually overrides an `Unclassified` Item
    /// to make it reclaimable (ADR-0001).
    pub override_reclaim: bool,
}

impl Item {
    /// Whether a [`crate::reclaim`] action is currently permitted on this Item.
    pub fn may_reclaim(&self) -> bool {
        self.class.is_reclaimable()
            || (self.class == SafetyClass::Unclassified && self.override_reclaim)
    }

    /// One-line "how you get it back" shown for the highlighted Item and quoted
    /// in the Confirm prompt. An overridden `Unclassified` Item has no Recovery
    /// Method of its own but goes to the Trash (ADR-0004), recoverable from there,
    /// so describe that destination rather than its `None` method.
    pub fn recovery_line(&self) -> String {
        if self.class == SafetyClass::Unclassified {
            return "moved to Trash on Reclaim — restorable from there".into();
        }
        self.recovery.describe()
    }
}

/// A whole Scan result: the classified Items plus the root that was walked.
#[derive(Debug, Default)]
pub struct Scan {
    pub root: PathBuf,
    pub items: Vec<Item>,
}

impl Scan {
    /// Total bytes reclaimable right now (excludes Protected and un-overridden
    /// Unclassified).
    pub fn reclaimable_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|i| i.may_reclaim())
            .map(|i| i.size_on_disk)
            .sum()
    }
}
