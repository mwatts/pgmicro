# Turso-Core Plan: Unpopulated materialized view state + `REFRESH MATERIALIZED VIEW`

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
follow-up pgmicro-side plan (wiring PostgreSQL's `WITH NO DATA` / `REFRESH
MATERIALIZED VIEW` syntax to the primitives added here) is not written yet and is
out of scope for this document.

## Problem

Turso already supports `CREATE MATERIALIZED VIEW ... AS SELECT ...` as **native SQL
grammar**, independent of any PostgreSQL dialect: `parser/src/parser.rs:837-851`
(`parse_create_materialized_view`) is reachable from plain Turso SQL, producing
`ast::Stmt::CreateMaterializedView` (`parser/src/ast.rs:175-183`), which
`core/translate/view.rs::translate_create_materialized_view` compiles into bytecode
that creates a backing btree plus a DBSP circuit (`core/incremental/view.rs`,
`IncrementalView`) and then **unconditionally** runs the view's full initial
population before the CREATE statement completes
(`core/translate/view.rs:253-256`, `Insn::PopulateMaterializedViews`).

Two capabilities are missing, and both are ordinary Turso engine gaps, not
PostgreSQL-specific ones:

1. **No way to create a materialized view without paying the initial-population
   cost immediately.** A materialized view over a large base table is expensive to
   populate; a caller who wants to bulk-load the base table first and populate the
   view once, afterward, currently cannot express that — `CREATE MATERIALIZED VIEW`
   always populates synchronously as part of the CREATE.
2. **No way to force a full recompute of an existing materialized view's contents.**
   Once created, a materialized view is only ever kept in sync incrementally (via
   the DBSP circuit reacting to writes on its referenced tables — see
   `IncrementalView::populate_from_table`/circuit machinery). There is no operation
   that says "discard whatever is there and recompute the view's contents from the
   current state of its referenced tables from scratch." That is a reasonable
   maintenance primitive independent of any specific SQL dialect (e.g. recovering
   from a suspected incremental-maintenance bug, or re-syncing after directly
   editing a referenced table through a path that bypassed the circuit).

Today, `core/pg_dispatch.rs:150-153` intercepts PostgreSQL's `REFRESH MATERIALIZED
VIEW` at the pg_query-protobuf level, before it ever reaches the shared
`ast::Stmt`/translator pipeline, and turns it into a hardcoded no-op
(`is_refresh_matview`, `parser_pg/src/translator.rs:5465-5475`), justified by a
comment: "Turso materialized views are live (auto-updating)". That justification
conflates two different things: incremental auto-update-on-write (which is real and
unaffected by this plan) with full-recompute-on-demand (which does not exist at
all today, live-updating or not). This plan adds the missing "create without
populating" and "recompute on demand" primitives as native Turso engine/SQL
features; it says nothing about how any particular SQL dialect's syntax maps onto
them.

## Design

### 1. New AST surface (dialect-agnostic, Turso-native grammar)

- `ast::Stmt::CreateMaterializedView` (`parser/src/ast.rs:175-183`) gains one new
  field: `populate: bool` (defaults to `true` for every existing caller/test —
  this is a pure additive field, see Migration/compatibility). When `false`, the
  view is created but its initial population is skipped.
- New grammar: `CREATE MATERIALIZED VIEW ... AS SELECT ... WITH NO DATA` sets
  `populate: false`. `WITH DATA` (or nothing) keeps the existing behavior
  (`populate: true`). Implemented in `parse_create_materialized_view`
  (`parser/src/parser.rs:837-851`) as an optional trailing clause after `select`.
- New statement: `ast::Stmt::RefreshMaterializedView { view_name: QualifiedName }`,
  parsed from `REFRESH MATERIALIZED VIEW <name>` by a new
  `parse_refresh_materialized_view`, dispatched from the top-level statement
  matcher the same way `TK_MATERIALIZED` already dispatches to
  `parse_create_materialized_view` (`parser/src/parser.rs:963`). Single view name
  only — no comma-separated list (there is no natural multi-view meaning here, and
  nothing today's grammar supports for other single-object DDL statements needs
  one either).
- New keyword tokens `REFRESH` and `DATA`, added the same way `MATERIALIZED`,
  `NO`, etc. already are: a `TokenType` variant (`parser/src/token.rs`), a lexer
  keyword-table entry (`parser/src/lexer.rs:102`, alongside the existing `"NO" =>
  TokenType::TK_NO`), and inclusion in the `fallback_id_if_ok` non-reserved-keyword
  list (`parser/src/token.rs:579-586`, where `TK_MATERIALIZED` and `TK_NO` already
  live) so `refresh` and `data` remain valid as ordinary identifiers everywhere
  else. This mirrors exactly how `MATERIALIZED` itself was added — there is no
  new category of grammar risk here, just two more contextual keywords.

**Why add real Turso grammar for this instead of an internal-only AST field with no
surface syntax:** `CREATE MATERIALIZED VIEW` itself is already exposed this way
(native grammar, no dialect involved), so leaving the new capability
grammar-less would be an inconsistent, half-finished version of the same feature.
A pgmicro-side follow-up plan (not written here) is responsible for making
`parser_pg/src/translator.rs` recognize PostgreSQL's own `WITH NO DATA` /
`REFRESH MATERIALIZED VIEW` syntax (via `pg_query`'s protobuf, e.g.
`IntoClause.skip_data` and `RefreshMatViewStmt`) and translate them into these same
`ast::Stmt` nodes, and for removing the current hardcoded no-op in
`core/pg_dispatch.rs:150-153`/`is_refresh_matview`. That translator wiring is
explicitly out of scope here.

### 2. Where the populated/unpopulated flag lives

Mirror the existing `incompatible_views: HashSet<String>` pattern
(`core/schema.rs:648`), which already tracks a per-materialized-view boolean
condition ("this view's on-disk DBSP state doesn't match the current circuit
version") entirely by name, separately from the `IncrementalView` struct itself:

- Add `pub unpopulated_views: HashSet<String>` to `Schema` (`core/schema.rs`,
  next to `incompatible_views`), threaded through the same places
  `incompatible_views` already is: the `Schema::default()`/`new()` initializer
  (`core/schema.rs:776,793`), and the clone-for-write-transaction path
  (`core/schema.rs:2229,2242`).
- Presence in the set means "not yet populated"; absence (the default, for every
  view created without `WITH NO DATA`, and for every view after its first
  successful `REFRESH`) means "populated." This directly encodes the required
  permanence rule: nothing ever re-inserts a view into this set except `CREATE
  ... WITH NO DATA` itself — a successful `REFRESH` removes it and nothing puts
  it back short of `DROP` + recreate. There is deliberately no "un-refresh"
  operation.
- **Durability across restarts:** unlike `incompatible_views` (which is
  *recomputed* at every schema load by comparing DBSP versions, `core/schema.rs`
  around line 1630, so it needs no persisted storage), "not yet populated" is a
  fact about history that cannot be recomputed from the view's SQL text alone — it
  must be persisted. Add one new internal table,
  `__turso_internal_matview_unpopulated` (fits the existing
  `RESERVED_TABLE_PREFIXES = ["sqlite_", "__turso_internal_"]` convention,
  `core/schema.rs:598`), schema `(view_name TEXT PRIMARY KEY)`. It is created
  lazily — only the first time any `CREATE MATERIALIZED VIEW ... WITH NO DATA`
  statement runs, exactly the way the per-view DBSP state table is created lazily
  per view today (`core/translate/view.rs:178-198`) — and a row is inserted for
  that view's name. At schema load, if the table exists, its rows populate
  `Schema::unpopulated_views` (same load pass that already reconstructs
  `materialized_view_sql`/`materialized_view_info`, `core/schema.rs:1842-1850`).
  `REFRESH` deletes the view's row (if present) as part of the same write
  transaction that repopulates it (see below); `DROP MATERIALIZED VIEW` deletes it
  too, alongside the existing cleanup of `materialized_view_names`/
  `materialized_view_sql` (`core/schema.rs:1224-1232`).
- Rejected alternative: piggybacking the flag on the per-view DBSP state table
  (`__turso_internal_dbsp_state_v<N>_<view>`) as a sentinel row. Rejected because
  that table's schema (`operator_id, zset_id, element_id, value, weight`,
  `core/translate/view.rs:184-192`) is circuit/operator computation state, not
  view metadata; overloading it risks a future DBSP operator claiming the sentinel
  `operator_id` and silently corrupting or losing the populated-state bit. A
  dedicated table keeps the two concerns (computation state vs. population
  status) unambiguous, at the cost of one small extra table that only exists at
  all once a `WITH NO DATA` view has ever been created.

### 3. What must check the flag, and what must not

- **Must error:** resolving a materialized view as a real, row-producing table
  reference — i.e. exactly the same place `incompatible_views` is already checked,
  `core/translate/planner.rs:1160-1174` (in the table-resolution path that runs
  when a query's `FROM` clause — or equivalent, e.g. an `UPDATE ... FROM`, a join
  side, a subquery source — names the view). Add a parallel check there: if
  `schema.unpopulated_views.contains(&normalized_qualified_name)`, error with
  `materialized view '{name}' has not been populated` (message text only — see
  note below on why this plan does not specify PostgreSQL's exact `HINT:` wire
  text).
  ```sql
  CREATE MATERIALIZED VIEW mv AS SELECT * FROM t WITH NO DATA;
  SELECT * FROM mv;              -- must error: not populated
  SELECT * FROM mv WHERE 1=0;    -- must error too — the error is about the view's
                                  -- state, not about whether rows would be returned
  ```
- **Must NOT error:** anything that only asks about the view's existence or shape
  without reading its row data. This falls out for free from checking at
  `planner.rs`'s FROM-clause table-resolution step rather than inside
  `Schema::get_materialized_view`/`is_materialized_view` (which many unrelated
  internal callers use, e.g. dependency tracking, `DROP`/`REFRESH` name
  resolution, schema introspection tables) — none of those paths route through
  `planner.rs`'s table-reference resolution for the view's own data, so none of
  them are affected by this check. Concretely, unaffected by design (not merely
  "should still work" but structurally cannot be broken by this change, since they
  never reach the new check):
  - `DROP MATERIALIZED VIEW mv` / `REFRESH MATERIALIZED VIEW mv` naming the view.
  - Any catalog/metadata table that lists materialized views by reading
    `Schema::materialized_view_names`/`materialized_view_sql` directly rather than
    scanning the view's own btree.
  - `sqlite_master`/`sqlite_schema` rows for the view (its DDL text and existence
    are always visible — only *querying its data* errors).

### 4. `REFRESH MATERIALIZED VIEW` semantics

- **Full replacement, not incremental**, matching the one primitive Turso already
  has for populating a materialized view's contents:
  `Insn::PopulateMaterializedViews` (`core/vdbe/insn.rs:1356`,
  `core/vdbe/execute.rs:11722` `op_populate_materialized_views`) recomputes the
  view from scratch by driving `IncrementalView::populate_from_table` from its
  `PopulateState::Start` state (`core/incremental/view.rs:24-77,1173+`). `REFRESH`
  reuses this exact instruction — it does not need a new computation path, only a
  new *caller* of the existing one:
  1. Clear the view's backing btree (the `Rewind`/`Delete`/`Next` loop already
     inlined in `translate_create_materialized_view`,
     `core/translate/view.rs:130-149` — extract this into a small shared helper
     used by both CREATE and REFRESH, since it would otherwise be duplicated
     verbatim).
  2. **Also clear the view's DBSP operator-state table**
     (`__turso_internal_dbsp_state_v<N>_<view>`) using the same
     Rewind/Delete/Next shape against that table's cursor. This is a design point
     that **needs verification before implementation, not an assumption**: at
     `CREATE` time this state table is freshly created empty
     (`core/translate/view.rs:96-101`), so `populate_from_table` has only ever
     been exercised starting from empty operator state. If any operator's state
     (e.g. running aggregate accumulators) is not idempotent when re-populated
     over pre-existing entries, skipping this clear would silently corrupt
     aggregate results on `REFRESH` — a correctness bug, not a performance
     concern. Confirm by reading `populate_from_table`'s use of
     `dbsp_state_root`/`dbsp_state_index_root` before implementing, and add a
     regression test: `REFRESH` a matview containing `SUM`/`COUNT` after the
     underlying table's data has changed, and assert the refreshed aggregate
     matches a fresh `CREATE MATERIALIZED VIEW` over the same final data (not the
     sum of old-plus-new).
  3. Emit `Insn::PopulateMaterializedViews` for this view's cursor, exactly as
     CREATE does (`core/translate/view.rs:253-256`).
  4. Delete the view's row from `__turso_internal_matview_unpopulated` if present,
     and remove it from `Schema::unpopulated_views` (no-op if the view was already
     populated — this is what gives the required permanence property for free:
     once removed, only a fresh `WITH NO DATA` create can add it back).
- **Populated state, once reached, is permanent** until `DROP` + recreate — see
  above; no code path re-adds a view to `unpopulated_views` except the CREATE-time
  `WITH NO DATA` insertion.
- **`CONCURRENTLY` is out of scope for this plan** — decided from evidence, not by
  default. Real `REFRESH MATERIALIZED VIEW CONCURRENTLY` avoids blocking readers
  by computing the new result set into a separate transient table and applying a
  minimal diff (via a required unique index) against the live table, rather than
  clearing and repopulating in place. Plain `REFRESH` as designed above reuses two
  primitives that already exist verbatim (the clear-loop, `PopulateMaterializedViews`)
  and runs inside the same single-writer write transaction every other DDL
  statement already uses — no new locking behavior. `CONCURRENTLY` has no
  equivalent existing primitive to reuse: it needs (a) a unique-index requirement
  check, (b) a temporary/shadow table to compute into without touching the live
  one, and (c) a row-level diff-and-patch step keyed by that unique index, none of
  which exist anywhere in `core/incremental/` or `core/translate/view.rs` today.
  That is new mechanism, not new wiring over old mechanism, so it does not meet
  this plan's bar of reusing what's already there — it is real follow-on work,
  flagged here rather than attempted.

### Files touched

- `parser/src/token.rs` — new `TK_REFRESH`, `TK_DATA` token variants; add both to
  the `fallback_id_if_ok` non-reserved-keyword list (~line 579-586).
- `parser/src/lexer.rs` — keyword-table entries for `"REFRESH"`/`"DATA"` (~line 102,
  1197).
- `parser/src/ast.rs` — `CreateMaterializedView` gains `populate: bool`; new
  `RefreshMaterializedView { view_name: QualifiedName }` variant.
- `parser/src/ast/fmt.rs` — `Display`/SQL-reconstruction updates for both.
- `parser/src/parser.rs` — extend `parse_create_materialized_view` (~line 837) for
  optional trailing `WITH NO DATA`/`WITH DATA`; add `parse_refresh_materialized_view`
  dispatched from the same top-level statement matcher as `TK_MATERIALIZED`
  (~line 963).
- `core/schema.rs` — new `unpopulated_views: HashSet<String>` field (next to
  `incompatible_views`, `core/schema.rs:648`), threaded through init/clone paths;
  load rows from `__turso_internal_matview_unpopulated` (if it exists) during
  schema load; remove from the set (and delete its tracking-table row) in the
  existing `DROP MATERIALIZED VIEW` cleanup path (`core/schema.rs:1224-1232`).
- `core/translate/view.rs` — extract the CREATE-time clear-loop into a shared
  helper; `translate_create_materialized_view`: read `populate`, and when
  `false`, skip emitting `Insn::PopulateMaterializedViews` and instead
  lazily-create (if missing) and insert into
  `__turso_internal_matview_unpopulated`; new
  `translate_refresh_materialized_view` implementing the REFRESH steps above.
- `core/translate/mod.rs` — dispatch `ast::Stmt::RefreshMaterializedView` to the
  new function (next to the existing `CreateMaterializedView` dispatch,
  `core/translate/mod.rs:247-254`).
- `core/translate/planner.rs` — add the "not populated" check alongside the
  existing `incompatible_views` check (~line 1160-1174).
- No new `Insn` variants: reused primitives are `Insn::PopulateMaterializedViews`,
  `Insn::Rewind`/`Delete`/`Next` (clearing), and ordinary `OpenWrite`/`Insert`/
  cursor operations for the new internal tracking table — all already exist.

## Explicitly out of scope

- **PostgreSQL syntax recognition** (`WITH NO DATA`, `REFRESH MATERIALIZED VIEW`
  parsed from `pg_query` protobuf) and removing the current
  `is_refresh_matview`/`core/pg_dispatch.rs:150-153` no-op shortcut — pgmicro-side
  translator work, a separate follow-up plan.
- **`REFRESH MATERIALIZED VIEW CONCURRENTLY`** — no reusable primitive exists for
  the required temp-table-plus-diff approach; see Design §4. Flagged as real
  future work, not silently dropped.
- **PostgreSQL's exact wire-level error shape** (`ERROR:`/`HINT:` two-part
  message). This plan defines a plain error message
  (`materialized view '{name}' has not been populated`) at the Turso-core level;
  mapping that into PostgreSQL's exact `SQLSTATE`/`HINT:` wire fields is a
  pg_server/pgwire-layer concern (`cli/pg_server.rs`), not addressed here.
- **Performance of `REFRESH`** — it is O(full recompute), matching
  `PopulateMaterializedViews`'s existing cost; no incremental-refresh
  optimization is proposed.
- **Multi-view `REFRESH MATERIALIZED VIEW a, b, c`** — not supported by the new
  grammar; single view name only (PostgreSQL itself has no multi-view form of
  this statement either, for what it's worth, though that fact is not the reason
  for the decision here — the reason is that nothing else in Turso's own DDL
  grammar needs it).

## Testing

- `cargo test -p turso_parser` (or the crate's actual parser test module) for the
  new `WITH NO DATA`/`WITH DATA` and `REFRESH MATERIALIZED VIEW` grammar,
  including round-trip `Display`/SQL-reconstruction tests.
- `cargo test -p turso_core` (schema/incremental-view tests) for:
  - `CREATE MATERIALIZED VIEW ... WITH NO DATA` leaves the view unpopulated (row
    present in `__turso_internal_matview_unpopulated`, `Schema::unpopulated_views`
    contains it) and does **not** run `PopulateMaterializedViews`.
  - Querying an unpopulated view's data errors with the exact message above;
    querying its existence/DDL (`sqlite_master`, `Schema::is_materialized_view`)
    does not error.
  - `REFRESH MATERIALIZED VIEW` on an unpopulated view: populates it, clears the
    "unpopulated" tracking row, and a subsequent `SELECT` succeeds and returns
    correct rows matching the current base-table contents.
  - `REFRESH MATERIALIZED VIEW` on an **already-populated** view after its base
    table has changed: result matches a fresh `CREATE MATERIALIZED VIEW` over the
    same final base-table state (this is the aggregate-state-reset regression
    test flagged in Design §4, step 2 — must be written and passing before this
    is considered done, not deferred).
  - Populated state survives connection close/reopen (persistence of both the
    view's data and the *absence* of an unpopulated-tracking row).
  - `DROP MATERIALIZED VIEW` on an unpopulated view cleans up its tracking row (no
    orphaned row if the same view name is recreated later).
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` and
  `cargo fmt`.
- Manual check via `cargo run -q --bin tursodb -- -q`:
  ```sql
  CREATE TABLE t(x INTEGER);
  INSERT INTO t VALUES (1), (2), (3);
  CREATE MATERIALIZED VIEW mv AS SELECT sum(x) AS total FROM t WITH NO DATA;
  SELECT * FROM mv;                        -- error: not populated
  REFRESH MATERIALIZED VIEW mv;
  SELECT * FROM mv;                        -- total = 6
  INSERT INTO t VALUES (10);
  SELECT * FROM mv;                        -- total = 16 (incremental maintenance still applies after populate)
  REFRESH MATERIALIZED VIEW mv;
  SELECT * FROM mv;                        -- total = 16, not 22 (proves DBSP state was reset, not double-applied)
  ```
  This is Turso-native SQL — no PostgreSQL/psql verification applies to this
  plan, unlike the JSON-containment plan's `@>`/`<@` semantics, which had to match
  an external system's documented (and non-obvious) behavior. The correctness bar
  here is entirely internal: does `REFRESH` produce the same result as a fresh
  `CREATE MATERIALIZED VIEW` over the same data.

## Migration / compatibility

- `CreateMaterializedView.populate` is additive with a `true` default; every
  existing caller (Turso's own parser, any code constructing this `Stmt` variant
  directly) is unaffected unless it explicitly sets `populate: false` or a future
  translator wires `WITH NO DATA` to it.
- `RefreshMaterializedView` is a wholly new statement; nothing depended on
  `REFRESH MATERIALIZED VIEW` being unparseable by Turso's own grammar (PostgreSQL
  callers currently never reach this grammar at all — see Problem).
- `__turso_internal_matview_unpopulated` only comes into existence the first time
  a `WITH NO DATA` view is created; databases with no such view have zero schema
  footprint change.
- No change to any existing materialized view's on-disk representation or
  behavior. This is the important compatibility fact worth stating plainly: every
  materialized view that exists today, or that anyone creates without `WITH NO
  DATA` after this lands, behaves exactly as it does now — populated immediately
  at CREATE, incrementally maintained thereafter. The only newly reachable
  behavior is for views explicitly opted into `WITH NO DATA`.
