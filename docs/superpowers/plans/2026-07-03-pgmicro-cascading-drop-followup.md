# pgmicro Plan: Wire `DROP ... CASCADE`/`RESTRICT` to the new dependency registry

**Depends on:** `2026-07-03-turso-core-cascading-drop.md` landing first (adds
the object-dependency registry + RESTRICT/CASCADE walk + NOTICE-per-object
mechanism to `core/`). Do not start this plan before that one merges — every
item below wires pgmicro syntax to a core primitive that does not exist yet.

## Context

Today, `parser_pg/src/translator.rs:966-969` (`translate_drop`) hard-rejects
`CASCADE` for every object type except `DROP SCHEMA`:

```rust
if DropBehavior::try_from(drop.behavior).ok() == Some(DropBehavior::DropCascade) {
    return Err(ParseError::ParseError(
        "DROP ... CASCADE is not supported; dependent objects must be dropped explicitly"
            .into(),
    ));
}
```

pinned by `test_drop_cascade_rejected_not_silently_ignored`
(`translator.rs:7362`). That is the correct interim choice — reject clearly
rather than silently ignore the keyword — but it means no PG migration script
or app that uses `DROP ... CASCADE` can run against pgmicro today.

Verified while writing this plan: `libpg_query` already parses the
RESTRICT/CASCADE distinction into `DropStmt.behavior` — the translator reads
it today only to reject it (`translator.rs:966`, and again for
`DropSequenceStmt`-adjacent handling at `translator.rs:5456`). None of
Turso's `ast::Stmt::Drop*` variants (`DropTable`/`DropIndex`/`DropView`/
`DropType`/`DropDomain`, `parser/src/ast.rs:241-275`) carry a cascade/behavior
field at all — confirmed by reading every variant. So this plan needs a new
AST field, not just a translator-side flag.

Also verified: there is no NOTICE-level channel anywhere in `cli/pg_server.rs`
or `core/` today (grepped both) — but the `pgwire` crate this project already
depends on (`pgwire-0.36.3`) has a `NoticeResponse` type with a direct
`From<ErrorInfo>` conversion (`pgwire::error::ErrorInfo`,
`pgwire/src/error.rs:234-236`) — the same `ErrorInfo` struct pgmicro's error
path already builds via `limbo_error_to_pg`
(`cli/pg_server.rs:1719-1775`). This means emitting a NOTICE is a small
addition once the core plan returns per-object notice text, not a new
protocol mechanism to invent.

## What this plan actually does

1. **New AST field, not a translate-time-only flag.** Add a `cascade: bool`
   (or a small `DropBehavior` enum, matching the existing PG-side type name
   for clarity) field to `DropTable`/`DropView`/`DropType`/`DropDomain` in
   `parser/src/ast.rs`. `DropIndex`/`DropTrigger` are out of scope (see Out of
   scope). This is a `parser/` change, not `parser_pg/` — flag for extra
   review per this project's "minimize core/ changes" principle, since
   `parser/` is shared with the SQLite dialect path (the field must default
   to non-cascade for all existing SQLite-dialect callers; verify no other
   caller of these AST variants needs updating — grep
   `ast::Stmt::DropTable`/`DropView`/etc. construction sites before landing).

2. **Translator wiring.** Replace the hard-reject block at
   `translator.rs:966-969` with: read `drop.behavior` into the new AST field
   instead of erroring. Remove `test_drop_cascade_rejected_not_silently_ignored`
   (`translator.rs:7362`) — its assertion (CASCADE errors) becomes false by
   design; replace it with new tests asserting CASCADE is accepted and
   produces the expected translated AST.

3. **NOTICE forwarding through the wire protocol.** Once the core
   dependency-walk returns (or streams) the list of objects it cascade-dropped,
   convert each into a `pgwire::messages::response::NoticeResponse` (via the
   existing `ErrorInfo`-construction path already used for errors in
   `cli/pg_server.rs`, reusing its SQLSTATE/severity-field shape but with
   severity `NOTICE` not `ERROR`) and send it on the client connection before
   the final `CommandComplete`. Check what `pgwire`'s `ClientInfo`/`Sink`
   traits expose for sending an out-of-band message mid-query — this needs
   investigation at implementation time (the core plan explicitly left open
   whether NOTICE emission belongs here or in core; this plan assumes core
   returns structured data and pgmicro does the wire encoding, consistent with
   "minimize core/ changes").

4. **REPL text output.** `pgmicro/src/main.rs`'s REPL loop needs the same
   per-object notice text surfaced as plain stdout lines (real `psql` prints
   `NOTICE:  drop cascades to view v1` above the `DROP TABLE` result) — check
   how the REPL currently prints any diagnostic/warning text, if any, and
   match that convention.

5. **Regression coverage for the RESTRICT-by-default bug fix.** The core plan
   found that `DROP TABLE`/`DROP VIEW` on an object with dependents *silently
   succeeds today* (no RESTRICT enforcement exists at all) — this is a
   pre-existing correctness bug independent of CASCADE. This plan's
   end-to-end tests must explicitly cover the **non-cascade** path erroring
   now where it silently succeeded before, not just the new CASCADE-succeeds
   path — a reviewer testing only "does CASCADE work" would miss confirming
   the silent-wrong-today bug is actually closed.

## Testing

Per this project's Core Principle 6 (REPL first, psql second):

```sql
-- REPL / integration test
CREATE TABLE t (id INTEGER);
CREATE VIEW v AS SELECT * FROM t;
DROP TABLE t;              -- expect: ERROR (was: silent success before this plan)
DROP TABLE t CASCADE;      -- expect: succeeds, NOTICE: drop cascades to view v, v dropped
SELECT * FROM v;           -- expect: ERROR, v no longer exists

CREATE TABLE parent (id INTEGER PRIMARY KEY);
CREATE TABLE child (id INTEGER, p_id INTEGER REFERENCES parent(id));
INSERT INTO child VALUES (1, 1);
DROP TABLE parent;          -- expect: ERROR (FK constraint depends on parent)
DROP TABLE parent CASCADE;  -- expect: succeeds, NOTICE naming the constraint, child + its rows UNCHANGED
```

Then the same two scripts through `psql` against the wire server, confirming
the `NOTICE:` lines actually arrive client-side (this is the part an
in-process Rust test cannot observe — the core plan's tests only prove the
core-side dependency walk and drop order, not that the NOTICE reaches a real
wire client).

- `cargo test -p turso_parser_pg` (translator tests: new CASCADE-accepted
  cases, removal of the now-inverted rejection test)
- `cargo test -p core_tester --test integration_tests integration::postgres`
  (end-to-end CASCADE/RESTRICT behavior)
- `cargo test -p pgmicro` (REPL notice-text output)
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`

## Out of scope

- `DROP INDEX`/`DROP TRIGGER ... CASCADE` — the core plan leaves these
  hard-rejected as today pending investigation of whether PG's dependency
  model even applies to them; this plan does not add AST fields for them
  either, to avoid unused surface.
- Exposing `pg_depend` as a queryable catalog table — the core plan flags
  this as a possible follow-up if `psql`'s own `\d+` introspection turns out
  to need it; if that recheck comes back "needed," it is a separate
  `core/pg_catalog.rs` task, not part of this wiring plan.
- Sequence/`DEFAULT nextval(...)`-dependency edges — blocked on the Sequences
  two-plan item landing first, per the core plan's own scoping.
- Exact NOTICE/ERROR wording — must be copied verbatim from a live `psql`
  session at implementation time, not guessed from this document or the core
  plan's memory-transcribed examples.

## Update to the original plan doc

`docs/superpowers/plans/2026-07-02-pgmicro-fixes.md`'s "Superseded" note for
the DROP CASCADE task now also names this file.
