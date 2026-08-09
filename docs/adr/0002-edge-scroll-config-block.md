# Edge-scroll parameters live in their own configuration block

The three edge-scroll parameters (band width, initial-delay, repeat-interval) are **promoted out of the drag configuration block into a dedicated edge-scroll block**, consumed by both drag edge-scroll and the new hover edge-scroll. The drag block retains only its drag-specific column-insert hit-band parameters. This was done because, once hover edge-scroll reuses the same band and cadence, those parameters are genuinely shared rather than drag-owned, and leaving them in the drag block overloads "drag edge-scroll" with "shared edge-scroll."

## Consequences

- This is a **breaking rename** of existing configuration keys. Existing user configs that customized the edge-scroll parameters under the drag block will silently revert to the compiled defaults unless migrated; there is no automatic migration layer today.
- The drag block is now self-contained for drag-only concerns (column-insert hit-banding), and the shared parameters have a home that documents their dual consumption.
