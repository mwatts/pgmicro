# pgmicro Plan: Wire `@>`/`<@`/`&&` to JSON-aware containment (Task B16 follow-up)

**Depends on:** `2026-07-03-turso-core-json-containment.md` landing first (adds
JSON/JSONB-aware containment to `array_contains_all`). Do not start this plan
before that one merges — landing the pgmicro-side wiring first would be a no-op
change riding on not-yet-existent core behavior.

## Context

Task B16 in `2026-07-02-pgmicro-fixes.md` originally proposed renaming the
translator's `@>`/`<@`/`&&` mapping from `array_contains_all`/`array_overlap` to
new `pg_contains`/`pg_overlaps` functions. That rename turned out to be
unnecessary: `parser_pg/src/translator.rs:2936-2953` (`translate_binary_expr`)
**already** maps `@>` → `array_contains_all(left, right)`, `<@` →
`array_contains_all(right, left)`, and `&&` → `array_overlap(left, right)` — the
same function names the Turso-core plan is extending. Once that core plan lands,
`@>`/`<@` on JSONB columns work correctly with **zero translator changes**,
because the translator was never wrong about which function to call — the
function itself just didn't handle JSON input yet.

## What this plan actually does

1. **Verification, not a rename.** After the core plan lands, confirm end-to-end
   behavior via the PG integration path (`cargo test -p turso_parser_pg`,
   `cargo test -p core_tester --test integration_tests integration::postgres`):
   ```sql
   CREATE TABLE t (doc jsonb);
   INSERT INTO t VALUES ('{"a":1,"b":2}');
   SELECT * FROM t WHERE doc @> '{"a":1}';   -- should match
   SELECT * FROM t WHERE doc <@ '{"a":1,"b":2,"c":3}';  -- should match
   ```
   Add these as new tests in `tests/integration/postgres/` if no existing test
   covers `@>`/`<@` on a real jsonb column (Task B16's original brief noted this
   gap was previously discovered via translator inspection only, not an
   end-to-end query test — close that gap here).

2. **`&&` on JSONB: reject, don't silently return NULL.** Real PostgreSQL has no
   `&&` operator for `jsonb` (`&&` is array/range overlap only — verified against
   PostgreSQL's operator catalog, `jsonb` only supports `@>`, `<@`, `?`, `?|`,
   `?&`, `||`, `-`, `#-`, `@?`, `@@`). The Turso-core plan deliberately does not
   extend `array_overlap` for JSON, so `&&` against a JSONB value still returns
   Turso's existing `NULL` from `array_overlap`'s un-parseable-input path — silent
   and easy to mistake for "no rows overlap" rather than "this operator doesn't
   apply here." Add a translate-time check in `translate_binary_expr`
   (`parser_pg/src/translator.rs:2936-2953`): this needs static type information
   the translator doesn't have (same limitation noted in the original B16 brief),
   so — consistent with this project's "reject unsupported syntax with a clear
   error over silently wrong results" rule — this cannot be fully fixed at
   translate time. Document the gap explicitly (doc comment at the `&&` arm)
   rather than leaving it silently wrong with no explanation, matching the
   precedent set by Task B18/B19 (CHAR/TIMETZ doc-only notes in the same plan).
   Do not attempt a runtime error path for this in translator code — that would
   need the same schema-lookup-hook architecture change flagged as out of scope
   for Task B12/B17 (`translator.rs:3123`).

3. **Update the original plan doc.** Mark Task B16 in
   `2026-07-02-pgmicro-fixes.md` as superseded by these two plans rather than
   leaving its "deferred, two-plan" text (and its now-inaccurate reference
   translation renaming to `pg_contains`/`pg_overlaps`) as the only record —
   a reader following the original doc today would implement the wrong fix.

## Testing

- `cargo test -p turso_parser_pg` (translator tests, unchanged mapping —
  regression only)
- `cargo test -p core_tester --test integration_tests integration::postgres`
  (new `@>`/`<@` end-to-end tests from step 1)
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`

## Out of scope

- Everything the Turso-core plan already excludes (no new operators, no
  `array_overlap` JSON extension, no perf work).
- Fixing `&&` against JSONB beyond documenting the gap (see item 2) — doing
  better requires the schema-lookup-hook architecture change that is its own,
  larger two-plan candidate (already tracked at `translator.rs:3123`).
