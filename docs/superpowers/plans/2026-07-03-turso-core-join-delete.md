# Turso-Core Plan: join-filtered DELETE (`DELETE ... USING`)

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
follow-up pgmicro-side plan (PG `USING` syntax parsing/translation) is not written
yet and is explicitly out of scope here.

## Problem

Turso's `DELETE` has no way to filter which rows to delete using a join against
other tables. Confirmed by reading the AST directly (`parser/src/ast.rs`, the
`Stmt::Delete` variant): it carries only `with`, `tbl_name`, `indexed`,
`where_clause`, `returning`, `order_by`, `limit` — no `from`/`using` field. SQLite
itself has no multi-table DELETE, which is presumably why this was never needed
before.

By contrast, `Stmt::Update` (`parser/src/ast.rs`) **already** has a
`from: Option<FromClause>` field, and `UPDATE ... FROM` is implemented end to end:
`core/translate/update.rs` (`prepare_update_plan`, `UpdatePlan::from_tables`),
`core/translate/emitter/update.rs` (`emit_program_for_update`), with passing tests
in `tests/integration/postgres/update_from.rs`. This is a real, working precedent
for "filter/compute a single-target-table DML statement using a join against other
read-only tables" — the same shape of problem `DELETE ... USING` needs to solve —
and the design below reuses its architecture rather than inventing a new one.

This is a generically useful Turso capability independent of PostgreSQL: any SQL
dialect with multi-table DELETE (PostgreSQL's `USING`, MySQL's multi-table
`DELETE`) needs the same underlying core support.

## Design

### AST change

Add a `from: Option<FromClause>` field to `Stmt::Delete`, mirroring `Stmt::Update`
exactly (same `FromClause` type, same position/meaning: a list of additional
tables/subqueries joined against the target table, readable from `where_clause`
and — new for DELETE — from `returning`).

### Planning: reuse `UpdatePlan`'s write-set architecture, but it needs a real fix, not a copy

`UpdatePlan` (`core/translate/plan.rs`) already models exactly this shape:
`target_table: JoinedTable` (the table being mutated) plus
`from_tables: TableReferences` (the read-only join graph), with a `DmlSafety`
mechanism (`DmlSafetyReason::UpdateFrom`, `core/translate/plan.rs:721-737`) that
forces a pre-materialization path when a `FROM` clause is present: a
`WriteSetPlan` (`core/translate/plan.rs:818-824`) runs the target-table-join-
from-tables SELECT first, buffering results into an ephemeral scratch BTree
(`core/translate/emitter/update.rs:130-166`), and only then does the write phase
run — reading from the scratch table instead of re-joining live, so table
mutation cannot invalidate the join's own iterator (Halloween-problem safety).
`DeletePlan` (`core/translate/plan.rs:758-782`) should gain the equivalent fields
(`from_tables: TableReferences`, `write_set_plan: Option<WriteSetPlan>`) and use
`DmlSafety`/`DmlSafetyReason` the same way — a `FROM`/`USING` clause on DELETE
should always force the write-set path for the same Halloween-problem reason it
does for UPDATE.

**This is not a drop-in reuse — the existing write-set population has an
unresolved row-multiplicity gap that DELETE cannot inherit.** Traced directly
(`core/translate/emitter/update.rs:90-166`): the scratch table is populated by
running the join SELECT and buffering every result row into a plain ephemeral
BTree (`OpenEphemeral`, ordinary auto-rowid table, no keying/dedup on target
rowid), then the write phase does a forward `Scan` over the scratch table with no
grouping. If a target row matches **N** from-table rows, the scratch table ends
up with **N** entries for that one target row, and the write phase iterates all
N. For `UPDATE ... FROM`, this is silently masked — the same target row gets
`SET` N times in a row, last write wins, and nothing currently visible breaks
(no test in `tests/integration/postgres/update_from.rs` exercises a multi-match
target row, so this has never been exercised, let alone asserted correct). For
`RETURNING`, it is not masked: `emit_program_for_update` buffers one RETURNING
row per write-phase iteration (`ReturningBufferCtx`, same file), so `UPDATE ...
FROM ... RETURNING` on a multi-match target row today would emit N returning
rows for one physically-updated row — divergent from real PostgreSQL's
documented single-row-per-target semantics, for both `UPDATE ... FROM` and,
if copied as-is, `DELETE ... USING`.

**Required fix, needed by this plan and worth fixing for `UPDATE ... FROM` too
(flag as a shared bug, not a DELETE-specific hack):** the write-set SELECT must
produce at most one row per target rowid before the write phase consumes it.
Options, in order of preference:
1. Group the write-set SELECT by target rowid (`GROUP BY <target rowid>`,
   picking an arbitrary/first matching row's join-side values per group) —
   matches real Postgres's own documented "arbitrary one of the matching
   rows" semantics for both `UPDATE ... FROM` and `DELETE ... USING`
   `RETURNING`.
2. Or: keep insertion order but make the scratch table keyed by target rowid
   (`INTEGER PRIMARY KEY` = target rowid) so a second match for the same
   target row overwrites rather than appends — cheaper to implement, same
   observable "last match wins, exactly one row survives" result.
Either way: **write a new test first** (multi-match target row through both
`UPDATE ... FROM` and the new `DELETE ... USING`, asserting exactly one
write/one RETURNING row) before implementing, since no such test exists today —
this is the single highest-risk correctness gap in this entire feature.

### Delete-specific semantics on top of the shared write-set fix

- **Existence, not values, drives the delete decision.** Unlike UPDATE (which
  needs the matched row's column values for `SET` expressions), a plain
  `DELETE ... USING ... WHERE` only needs "does at least one matching row
  exist" per target row — a semi-join. Once the write-set dedup fix above is in
  place, this falls out for free: the (deduped) write-set scan simply drives
  which target rowids get deleted, exactly as the existing `DeletePlan`'s
  `rowset_plan`/`rowset_reg` pre-materialization (`core/translate/plan.rs:775-778`)
  already drives ordinary single-table DELETE's rowid collection — extend that
  same rowid-collection idea to read rowids out of the write-set scratch table
  instead of a live single-table scan when `from_tables` is present.
- **`USING`-table rows are read-only.** No writes, no cursor-invalidation
  concerns for those tables — they are pure join input, exactly like `FROM` on
  `UPDATE`. Nothing new needed beyond what `TableReferences`/`from_tables`
  already models.
- **Self-referential `USING`** (`DELETE FROM t USING t AS t2 WHERE t.x = t2.y
  AND t.id <> t2.id` — the standard "delete duplicate rows" idiom) must work.
  Verify by reusing the same self-join mechanism `UPDATE ... FROM` already
  supports for a table referenced as both target and FROM source (check
  whether `update_from.rs`'s test suite covers a self-join target/FROM case —
  it does not currently; add one for both UPDATE and DELETE, since the cursor
  aliasing needed for target-vs-FROM-alias-of-same-table has not been
  exercised by any existing test). This needs its own real-`psql` cross-check:
  confirm PostgreSQL really deletes the expected "second occurrence of each
  duplicate" and not both/zero rows for a small worked example before trusting
  any test's expected values.
- **`RETURNING` with `USING`:** after the write-set dedup fix, RETURNING
  projects columns from the target row plus (if referenced) the one surviving
  matched USING-row's columns per the write-set's chosen tie-break (arbitrary
  match) — same mechanism `UPDATE ... FROM ... RETURNING` already uses via
  `write_phase_tables` keeping the target table as an outer reference
  (`core/translate/emitter/update.rs:135-145`) so RETURNING can read both target
  and scratch-table columns in the write phase.
- **Row-count reporting:** the target table's own row count (`DELETE 3`, not a
  join-multiplied count) — guaranteed by construction once the write-set is
  deduped by target rowid; call out explicitly as a thing the multi-match test
  must assert, not just RETURNING row count.

## Files touched

- `parser/src/ast.rs` — add `from: Option<FromClause>` to `Stmt::Delete`.
- `parser/src/parser.rs` (or wherever `Stmt::Delete` is constructed from grammar)
  — thread the new field through, defaulting to `None` for existing DELETE
  parsing paths (no behavior change when absent).
- `core/translate/plan.rs` — `DeletePlan`: add `from_tables: TableReferences`,
  `write_set_plan: Option<WriteSetPlan>`; extend `DmlSafetyReason` usage so a
  present `from_tables` forces the write-set path (mirroring
  `DmlSafetyReason::UpdateFrom`).
- `core/translate/delete.rs` — `translate_delete`/plan-building: build
  `from_tables` from the new AST field the same way
  `prepare_and_optimize_update_plan` does (`core/translate/update.rs:160-196`);
  wire subquery planning (`plan_subqueries_from_where_clause`) and
  `plan_subqueries_from_returning` against the joined `from_tables`, not just
  the target table, since `RETURNING` can now reference USING-table columns.
- `core/translate/emitter/delete.rs` — `emit_program_for_delete` (or equivalent):
  add the write-set population/consumption path analogous to
  `emit_program_for_update` (`core/translate/emitter/update.rs:90-166`),
  **including the row-multiplicity fix** — do not port the existing
  ungrouped/unkeyed scratch-table population verbatim.
- `core/translate/plan.rs` and/or `core/translate/emitter/update.rs` — apply the
  same write-set dedup fix to the existing `UPDATE ... FROM` path (shared root
  cause; fixing it only for the new DELETE path while leaving `UPDATE ... FROM`
  with the same latent bug would be inconsistent and leaves a known bug
  undocumented elsewhere).
- Tests: `core/translate` unit tests for the new `DeletePlan` fields; a new
  Rust-API integration test file (e.g.
  `tests/integration/postgres/delete_using.rs`, mirroring the structure of
  `tests/integration/postgres/update_from.rs`) covering: basic single-match
  delete, no-match (target row survives), multi-table `USING` (two joined
  tables), multi-match target row (the dedup fix — assert exactly one row
  deleted, exactly one RETURNING row, and check which columns a real `psql`
  run says survive in RETURNING before hardcoding the expectation),
  self-referential `USING` (delete-duplicates idiom), and `RETURNING` pulling
  columns from both target and USING tables.

## Explicitly out of scope

- **PG `DELETE ... USING` syntax parsing/translation**
  (`pg_query`-protobuf → this new AST field, in `parser_pg/src/translator.rs`):
  belongs in the pgmicro-side follow-up plan, not written as part of this
  Turso-core plan.
- **MySQL-style multi-table `DELETE FROM t1, t2 USING ...` (deleting from
  multiple tables in one statement):** PostgreSQL's `USING` only ever deletes
  from one target table; this plan matches that scope. A future dialect
  wanting true multi-target delete is a separate, larger feature.
- **`ORDER BY`/`LIMIT` combined with `USING`:** real PostgreSQL's `DELETE` does
  not support `ORDER BY`/`LIMIT` at all (that's a MySQL extension some engines
  allow); no new interaction to design here beyond what already exists.
- **Performance tuning of the write-set scratch table** (indexing it, avoiding
  the extra materialization when no `RETURNING`/multi-match is possible):
  correctness first; flag as a follow-up once the dedup fix and semantics are
  verified correct.

## Testing

- `cargo test -p turso_core` for the new `DeletePlan`/emitter unit tests.
- `cargo test -p core_tester --test integration_tests integration::postgres` for
  the new `delete_using.rs` integration tests (and the amended
  `update_from.rs` multi-match test).
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` and
  `cargo fmt` per standard workflow.
- **Every semantic claim above must be independently checked against a live
  `psql` instance before being trusted**, in particular:
  ```sql
  -- multi-match target row + RETURNING: exactly one row, which USING-side values?
  CREATE TABLE t (id int primary key, val int);
  CREATE TABLE s (id int, tag text);
  INSERT INTO t VALUES (1, 10);
  INSERT INTO s VALUES (1, 'a'), (1, 'b');
  DELETE FROM t USING s WHERE t.id = s.id RETURNING t.id, s.tag;
  -- real psql: exactly one row back, with an arbitrary (implementation-defined)
  -- s.tag value -- confirm this instead of assuming, and confirm exactly one
  -- row, not two.

  -- self-referential USING (delete-duplicates idiom)
  CREATE TABLE d (id int, val int);
  INSERT INTO d VALUES (1,'x'... -- adjust types
  DELETE FROM d USING d AS d2 WHERE d.val = d2.val AND d.id < d2.id;
  -- confirm which of the two duplicate rows real PG actually deletes
  -- (the doc-quoted idiom, not assumed) before hardcoding an expected id in a test.
  ```

## Migration / compatibility

Additive: `Stmt::Delete.from` defaults to `None`, existing single-table DELETE
parsing/planning/emission is unchanged when absent. The shared write-set dedup
fix changes `UPDATE ... FROM` behavior only for the previously-untested
multi-match case (arguably a bug fix, not a behavior change users could have
been relying on, since no test asserts the old N-rows-returned behavior). No
schema/storage format change.
