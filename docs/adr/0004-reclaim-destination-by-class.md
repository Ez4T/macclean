# Reclaim destination depends on Safety Class

Reclaiming one of the four Reclaimable classes (Regenerable, Reinstallable, Cache,
Redundant Copy) deletes permanently, freeing space immediately — these are
recoverable by construction, so there is nothing to hedge. Reclaiming a manually
overridden **Unclassified** item instead moves it to the Trash, because that is the
one path where the user deletes something the app could not vouch for, and a
same-volume safety net is worth more there than immediate space. We chose this
split so the app deletes confidently exactly where it has evidence and hedges
precisely where it does not — mirroring the fail-safe stance of ADR-0001.

## Consequences

- Trashing Unclassified items does not free space until the Trash is emptied; this
  is surfaced to the user as the cost of the safety net.
- Reclaim is not a single uniform operation; its destination is a function of the
  Item's Safety Class.
