# pgmicro Plan: Wire `WITH NO DATA` / `REFRESH MATERIALIZED VIEW` to Turso's unpopulated-matview primitives

**Depends on:** `2026-07-03-turso-core-unpopulated-matview.md` landing first
(adds `ast::Stmt::CreateMaterializedView.populate`, the new
`ast::Stmt::RefreshMaterializedView` statement, `Schema::unpopulated_views`,
and the not-populated check in `core/translate/planner.rs`). Do not start
wiring pgmicro syntax to these until they exist — there is nothing to wire to
yet.

## Context

Two things are broken today, independent of whether the core plan has
landed, and both are worth fixing precisely because they are currently
**silently wrong**, not merely unimplemented:

1. **`CREATE MATERIALIZED VIEW ... WITH NO DATA` is silently ignored today.**
   `translate_create_table_as` (`parser_pg/src/translator.rs:1092-1145`) reads
   `into_clause.rel` and `into_clause.col_names` from PG's `CreateTableAsStmt`
   protobuf, but never reads `into_clause.skip_data` — confirmed by reading
   the function in full and grepping the whole file for `skip_data`: zero
   matches. `pg_query`'s generated protobuf does carry this field
   (`IntoClause.skip_data: bool`, `pg_query-6.1.1/src/protobuf.rs:739`), so
   `CREATE MATERIALIZED VIEW mv AS SELECT ... WITH NO DATA` parses fine today
   and is translated **as if `WITH NO DATA` had not been written at all** —
   the view is created and fully populated immediately, the opposite of what
   the caller asked for. **Note:** the master plan (`2026-07-02-pgmicro-fixes.md`,
   Task B24) already specs a fail-loud fix for exactly this — rejecting
   `skip_data` with a clear `ParseError` instead of silently ignoring it —
   but as of this writing that task's checkbox is unchecked and the rejection
   code is not present in `translate_create_table_as`; confirmed by direct
   read, not assumed from the plan text. If Task B24 lands before this plan
   starts, item 1 below changes from "read a previously-ignored field" to
   "replace B24's reject-with-error with the real `populate: false`
   translation" — same end state either way, just note which starting point
   applies when this work begins.
2. **`REFRESH MATERIALIZED VIEW` is a hardcoded no-op regardless of
   `CONCURRENTLY`/`WITH NO DATA`.** `core/pg_dispatch.rs:151-154` intercepts
   every `RefreshMatViewStmt` at the protobuf level, before the translator or
   `ast::Stmt` pipeline ever sees it, and always returns `SELECT 0 WHERE 0`
   (`is_refresh_matview`, `parser_pg/src/translator.rs:5465-5475`), justified
   by a comment: "Turso materialized views are live (auto-updating)." That
   justification is accurate for incremental auto-update on writes to the
   base table, but says nothing about a caller who explicitly asks to force a
   full recompute — today there is no way to do that at all, and the no-op
   silently discards the request rather than saying so.

Once the core plan lands, both gaps have a real primitive to wire to; this
plan is that wiring, plus a matching wire-protocol error path.

## What this plan actually does

1. **Parse `WITH NO DATA` / `WITH DATA` and set `populate` accordingly.** In
   `translate_create_table_as`, read `into_clause.skip_data`
   (`pg_query::protobuf::IntoClause`, confirmed field at
   `pg_query-6.1.1/src/protobuf.rs:739`) and set the new
   `ast::Stmt::CreateMaterializedView.populate` field to `!skip_data`. This is
   the only change needed on the CREATE side — no new AST types, no new
   translator dispatch, just reading a field that was already being silently
   dropped.

2. **Translate `RefreshMatViewStmt` into `ast::Stmt::RefreshMaterializedView`
   instead of intercepting it as a no-op.** Remove the
   `is_refresh_matview`/`core/pg_dispatch.rs:151-154` short-circuit entirely —
   once the core AST node exists, `RefreshMatViewStmt` should flow through the
   normal translate path like every other statement, not be caught earlier.
   Add a `translate_refresh_matview` in `parser_pg/src/translator.rs` that
   reads `RefreshMatViewStmt.relation` (`pg_query-6.1.1/src/protobuf.rs:4203-4210`)
   into the view name and produces `ast::Stmt::RefreshMaterializedView { view_name }`.

3. **Reject `CONCURRENTLY` and `WITH NO DATA` on `REFRESH`, don't silently
   drop them.** `RefreshMatViewStmt` carries its own `concurrent: bool` and
   `skip_data: bool` fields (`pg_query-6.1.1/src/protobuf.rs:4204-4207`) —
   real PostgreSQL supports both `REFRESH MATERIALIZED VIEW CONCURRENTLY mv`
   and `REFRESH MATERIALIZED VIEW mv WITH NO DATA` (the latter clears the
   view's data and marks it unpopulated again, without recomputing). The core
   plan's `ast::Stmt::RefreshMaterializedView` has neither field — it only
   models plain, blocking, full-repopulate REFRESH (see core plan Design §4,
   which explicitly scopes `CONCURRENTLY` out for lack of a reusable
   diff/temp-table primitive). This translator must therefore reject both
   `concurrent` and `skip_data` on `RefreshMatViewStmt` with a clear
   `ParseError` naming the unsupported clause, rather than silently
   translating them as plain REFRESH (which would be wrong: plain REFRESH
   after a `WITH NO DATA` refresh request repopulates the view instead of
   leaving it empty, and after a `CONCURRENTLY` request blocks readers instead
   of not). If `REFRESH ... WITH NO DATA` support is wanted later, it needs
   its own small core-plan amendment (the `RefreshMaterializedView` AST node
   would need a `populate: bool` field mirroring `CreateMaterializedView`'s) —
   flagged here as a known gap, not silently worked around by reusing the
   existing node.

4. **Map the "not populated" error to the correct PostgreSQL SQLSTATE.** The
   core plan's `planner.rs` check (Design §3) raises a plain-text error;
   pgmicro's existing SQLSTATE-classification pattern
   (`classify_planning_sqlstate`, `cli/pg_server.rs:1782-1795`) already
   substring-matches `PlanningError` message text to pick a code (e.g. `"no
   such table"` → `42P01`). Add a branch there: message containing `"has not
   been populated"` → SQLSTATE `55000` (class 55, "object not in
   prerequisite state"). **Verify against real `psql`/PostgreSQL before
   relying on this** — this is the documented real-PG behavior for querying
   an unpopulated matview (`ERROR: materialized view "mv" has not been
   populated` / `HINT: Use the REFRESH MATERIALIZED VIEW command.`, SQLSTATE
   class 55), but has not been independently re-confirmed against a live
   server in writing this plan; do not skip that check before implementing.
   `HINT:` text is message-only per pgwire's error-field support — check
   whether `cli/pg_server.rs`'s error-response encoding carries a `HINT`
   field at all before promising one.

5. **End-to-end tests, not just translator unit tests.** The existing
   translator tests at `parser_pg/src/translator.rs:8682-8735` (`CREATE
   MATERIALIZED VIEW`, `CREATE MATERIALIZED VIEW IF NOT EXISTS`, `DROP
   MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW`) only assert that these
   statements parse/translate without error — none of them exercise `WITH NO
   DATA`, and the existing `REFRESH MATERIALIZED VIEW` test necessarily
   predates real REFRESH semantics since the statement was a no-op. Add new
   integration tests (`tests/integration/postgres/`) that actually run the
   full round trip:
   ```sql
   CREATE TABLE t(x INTEGER);
   INSERT INTO t VALUES (1), (2), (3);
   CREATE MATERIALIZED VIEW mv AS SELECT sum(x) AS total FROM t WITH NO DATA;
   SELECT * FROM mv;                    -- expect ERROR, SQLSTATE 55000
   REFRESH MATERIALIZED VIEW mv;
   SELECT * FROM mv;                    -- expect total = 6
   REFRESH MATERIALIZED VIEW CONCURRENTLY mv;  -- expect ERROR, clear rejection
   ```

## Testing

Per this project's Core Principle 6 ("test with the REPL first, psql
second"): drive all of the above via `cargo run -p pgmicro -- :memory:`
first, then verify the SQLSTATE/error-text shape via `psql` against the
`--server` wire mode as a second pass, not the primary path.

- `cargo test -p turso_parser_pg` — new tests for `WITH NO DATA` translation
  (`skip_data` → `populate: false`) and for `RefreshMatViewStmt` → `ast::Stmt::RefreshMaterializedView`,
  plus new tests asserting `CONCURRENTLY`/`WITH NO DATA` on REFRESH are
  rejected with a clear error, not silently accepted.
- `cargo test -p core_tester --test integration_tests integration::postgres`
  — the end-to-end round trip from item 5 above.
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets --
  --deny=warnings`.

## Out of scope

- Everything the core plan already excludes: `CONCURRENTLY` support itself
  (rejected here, not implemented), performance of `REFRESH` (O(full
  recompute) is the accepted behavior), multi-view `REFRESH a, b, c`.
- `REFRESH MATERIALIZED VIEW ... WITH NO DATA` support — flagged above as
  needing its own small core-plan amendment before it can be wired; this plan
  only rejects it cleanly.
- Real PostgreSQL's exact `HINT:` wire text — only the `ERROR:` message and
  SQLSTATE are targeted here; whether pgwire's error-response encoding even
  carries a HINT field needs checking before promising one (see item 4).

## Update to the original master plan

`docs/superpowers/plans/2026-07-02-pgmicro-fixes.md`'s "Superseded" note
for the `WITH NO DATA`/matview task should also name this file
(`2026-07-03-pgmicro-unpopulated-matview-followup.md`) as the pgmicro-side
follow-up, alongside its existing reference to
`2026-07-03-turso-core-unpopulated-matview.md`.
