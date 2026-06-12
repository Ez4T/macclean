# Unrecognized items default to Unclassified, never auto-offered

When the classifier has no rule for a large Item, it assigns the **Unclassified**
Safety Class rather than guessing a Reclaimable class. Unclassified items are
surfaced prominently (so big unknowns are never hidden) but are never offered for
deletion automatically — the user must inspect and explicitly override before
they can be Reclaimed. We chose this because the cost of wrongly deleting
irreplaceable data (a stray database file, a documents tree) vastly outweighs the
inconvenience of an unrecognized-but-reclaimable directory going unoffered, and
because the app's entire value rests on being trustworthy enough to run without
second-guessing it.

## Consequences

- Unlike typical cleaners, the app will *not* proactively suggest deleting large
  files it doesn't understand. Users wanting that space back must opt in per item.
- Coverage of the classifier's ruleset directly determines how much the app can
  offer; gaps surface as Unclassified rather than as risk.
