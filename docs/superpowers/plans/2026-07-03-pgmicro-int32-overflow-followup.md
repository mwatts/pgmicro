# pgmicro Plan: Wire `int4`/`integer` to the new overflow-checked type (INT4 follow-up)

**Depends on:** `2026-07-03-turso-core-int32-overflow.md` landing first (adds
the `int4` custom type with checked `+`/`-`/`*`/`/` operators and a 32-bit
`ENCODE` range check). Do not start this plan before that one merges.

## Context

The core plan found `int4`/`integer` has no custom type at all today —
`parser_pg/src/translator.rs:4853` maps `INTEGER`/`INT`/`INT4`/`SERIAL`/
`SERIAL4`/`BIGSERIAL`/`SERIAL8`/`SMALLSERIAL`/`SERIAL2` all straight to the raw
base type `"INTEGER"`, with no wrapper and no range check. Once the core
plan's `int4` type lands, this plan wires PG's `integer` column declarations
onto it and fixes the two pre-existing gaps the core plan explicitly left out
of its own scope.

## What this plan actually does

1. **Repoint bare `INTEGER`/`INT`/`INT4` to the new `int4` type — but not
   `SERIAL*`.** In `translate_pg_type_to_turso` (`translator.rs:4853`), change
   the match arm so `"INTEGER" | "INT" | "INT4"` map to `"int4"` while
   `"SERIAL" | "SERIAL4" | "BIGSERIAL" | "SERIAL8" | "SMALLSERIAL" | "SERIAL2"`
   keep mapping to plain `"INTEGER"`, split into two separate match arms.

   **Verify before implementing, do not assume:** Turso has a
   `rowid_alias`/`F_ROWID_ALIAS` mechanism (`core/schema.rs:4550`,
   `is_rowid_alias()`; flag set from `ColDef.rowid_alias` at
   `core/schema.rs:4483-4485`) that makes a single-column `INTEGER PRIMARY
   KEY` an alias for SQLite's rowid — this requires the column's declared
   type to resolve to exactly the base `INTEGER` affinity, per SQLite's own
   rule. SERIAL columns are marked primary-key + not-null by
   `translator.rs`'s `is_serial_type` handling (~line 395-406) specifically so
   they get this rowid-alias fast path. Trace where `ColDef.rowid_alias` is
   actually computed from the parsed declared-type text (not located during
   this write-up — grep `rowid_alias` across `core/translate/` and the
   `CREATE TABLE` column-parsing path) and confirm a custom-type name like
   `int4` would **not** silently defeat rowid-alias detection before touching
   SERIAL's mapping at all. If confirmed safe, still leave SERIAL as plain
   `INTEGER` per the split above — there is no PG behavior requiring
   SERIAL's *storage* to be 32-bit-checked (SERIAL is really just a default
   value with the type being `int4`/`int8`; the checked-arithmetic behavior
   comes from `integer` itself, so mapping `SERIAL` to `int4` would double up
   with the primary-key/rowid-alias fast path for no behavioral gain). Do not
   change the SERIAL mapping unless a concrete PG-observable gap is found
   that requires it.

2. **`BIGINT`/`INT8`/`BIGSERIAL` are explicitly out of this plan's scope.**
   Real PostgreSQL `bigint` is 64-bit and already fits Turso's native
   `INTEGER` storage range — only `int4`'s 32-bit bound is newly strict.
   Confirm the translator's `BIGINT`/`INT8` mapping is untouched by this
   change (it is a separate match arm from the one edited in step 1) and add
   a regression test proving bigint arithmetic at `int4`-overflow magnitudes
   does **not** error (see Testing below) — this is the boundary the core
   plan's design explicitly relies on ("plain `int8`/untyped-integer
   arithmetic has no operand that resolves to this type def").

3. **Fix the `int4(x)`/`int2(x)`/`int8(x)` cast-shortcut bypass.**
   `translator.rs:3473-3479` currently translates `int4(x)`/`int2(x)`/
   `int8(x)` function-call syntax directly to `CAST(x AS INTEGER)`, bypassing
   even the existing `smallint` custom type entirely (`int2(40000)` doesn't
   range-check today — a pre-existing, separate gap the core plan flagged
   as explicitly out of its scope and left for here). Change this arm so
   `INT4` casts to `"int4"` and `INT2` casts to `"smallint"` (the existing
   type); leave `INT8` casting to plain `"INTEGER"` per point 2 above.

4. **Unary negation overflow — resolve the core plan's open question, don't
   leave it ambiguous.** The core plan flags `SELECT -(-2147483648::int4);`
   as bypassing the `OPERATOR` dispatch mechanism entirely (unary expressions
   translate via a separate path, `core/translate/expr.rs:3920`, that never
   calls `find_custom_type_operator`) and explicitly declines to pick a
   resolution, calling it a human decision. **Decision for this plan: accept
   the gap for a first version rather than requesting the small
   parser/typesystem extension the core plan describes as option (a).**
   Rationale: this is a narrow edge case (only the exact value `i32::MIN`
   negated) with a well-understood, verifiable-any-time trigger, and forcing
   a `core/`/`parser/` change (state which the core plan says grows the
   change from "zero" to "small") into a follow-up plan whose entire premise
   is "the core plan is done, this is wiring only" would blur that boundary.
   Document the gap explicitly with a doc comment where the cast-shortcut
   and column-type mapping are edited (steps 1 and 3 above), matching the
   `&&`-on-jsonb precedent in
   `2026-07-03-pgmicro-json-containment-followup.md` (document rather than
   silently leave wrong), and add an explicit `#[ignore]`d or clearly-labeled
   known-gap test for `SELECT -(-2147483648::int4);` rather than omitting
   coverage of it entirely. If real usage later shows this matters, revisit
   as its own small core-plan addendum.

5. **SQLSTATE mapping needs no new work — verified already correct.**
   `core/error.rs:59` already defines `LimboError::IntegerOverflow`, and
   `cli/pg_server.rs:1745` already maps it to SQLSTATE `22003` in
   `limbo_error_to_pg` (confirmed by direct read — this is real
   PostgreSQL's `numeric_value_out_of_range` code, matching the core plan's
   `int4_add`/etc. functions' expected error surface). The new `int4`
   scalar functions added by the core plan should return
   `LimboError::IntegerOverflow` on `checked_*` failure, reusing this
   existing variant and mapping rather than adding a new one. Confirm this
   end to end via the wire protocol (`psql`), not just the in-process API,
   since `cli/pg_server.rs:1392`/`:1397` already show this exact variant
   being raised for `i16`/`i32` conversion failures elsewhere in the same
   file — a working precedent to follow, not new ground.

## Testing

Per this project's Core Principle 6: test with the REPL first
(`cargo run -p pgmicro -- :memory:`), psql second.

```sql
CREATE TABLE t (a integer);
INSERT INTO t VALUES (2147483647);
SELECT a + 1 FROM t;                          -- ERROR: integer out of range
SELECT 2147483647::int4 + 1;                  -- ERROR: integer out of range
INSERT INTO t VALUES (9999999999);             -- ERROR at INSERT/assignment time, not just a CHECK
SELECT int4(9999999999);                       -- ERROR: integer out of range (cast-shortcut path, step 3)
SELECT int2(40000);                            -- ERROR: integer out of range (now routes through smallint)

CREATE TABLE big (a bigint);
INSERT INTO big VALUES (2147483647);
SELECT a + 1 FROM big;                         -- 2147483648, NO error (bigint boundary proof, step 2)

CREATE TABLE s (id SERIAL PRIMARY KEY);
INSERT INTO s DEFAULT VALUES;
INSERT INTO s DEFAULT VALUES;
SELECT id FROM s ORDER BY id;                  -- 1, 2 — confirm rowid-alias autoincrement still works
                                                -- after step 1's split (this is the regression the
                                                -- "verify before implementing" callout above exists to catch)

SELECT -(-2147483648::int4);                    -- documented known gap (step 4): currently does NOT error
```

Run against `cargo test -p turso_parser_pg` and
`cargo test -p core_tester --test integration_tests integration::postgres`
for the new end-to-end cases; `cargo fmt`,
`cargo clippy --workspace --all-features --all-targets -- --deny=warnings`.

## Out of scope

- Everything the core plan's own "Explicitly out of scope" section excludes
  (no `smallint`/`bigint` arithmetic-operator upgrade, no `numeric` changes).
- Fixing unary negation overflow — explicitly deferred as a documented gap
  per step 4, not solved here.
- `pg_catalog.rs`'s existing `int4` OID-23 `pg_type` row
  (`core/pg_catalog.rs:2444-2446`) is unrelated to this plan and untouched —
  it already exists for wire-protocol/introspection purposes independent of
  which Turso custom type backs the column.

## Update to the original plan doc

`docs/superpowers/plans/2026-07-02-pgmicro-fixes.md`'s existing "Superseded"
note for INT4 (in the Workstream D sequencing notes, referencing
`2026-07-03-turso-core-int32-overflow.md`) should also name this follow-up
doc — see the accompanying edit to that file.
