# Classification rules are data, not code

The classifier is driven entirely by a declarative **Ruleset**: each **Rule** is a
data entry mapping a match condition to a Safety Class (and a clean command for the
Reclaimable build/dependency classes). The app ships a curated default ruleset as
bundled data, and users can add or override Rules via config. We chose this over
hardcoded detectors because the classifier is the trust-critical core of the app —
a user must be able to audit *why* an Item was labeled Regenerable, and teaching the
app about a new toolchain or cache path must be a data change, not a code release.

## Consequences

- Built-in and user rules use the same mechanism; there is no privileged hidden
  ruleset to keep in sync.
- The Rule schema (match conditions, Safety Class, clean command, evidence to
  display) becomes a stable interface that constrains future classifier features.
- A malformed or overly broad user Rule could mislabel Items; rule evaluation must
  stay conservative and the fail-safe to Unclassified (ADR-0001) is the backstop.
