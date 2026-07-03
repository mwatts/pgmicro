# Turso-Core Plan: object-dependency tracking for DROP (RESTRICT/CASCADE)

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency
in its rationale (a generic "what depends on this object" registry is useful to any
SQL dialect Turso serves). This is plan (1) of a two-plan pair; the pgmicro-side
follow-up plan (wiring `DROP ... CASCADE` support back on, and adding
`NOTICE: drop cascades to ...` messages) is a separate document, not yet written —
do not start it before this one lands.

## Acceptance bar

This codebase's purpose is reproducing PostgreSQL behavior on top of Turso. A
"reasonable-sounding generic dependency-tracking design" is not the bar — matching
real `psql`'s RESTRICT/CASCADE/NOTICE output exactly is. Every behavioral claim
below is transcribed from documented PostgreSQL semantics (`DROP TABLE`/`DROP
VIEW` reference pages, the `pg_depend` dependency model), but **must be
spot-checked against a live `psql` instance before this is considered done** —
in particular the exact wording of RESTRICT error text and CASCADE NOTICE text,
which this document does not have the ability to verify from memory with
certainty and must not be trusted over an actual server's output.

## Problem (verified by reading the code, not assumed)

### 1. `CASCADE` is a hard rejection for every object type except SCHEMA

`parser_pg/src/translator.rs:966-969` (`translate_drop`):

```rust
if DropBehavior::try_from(drop.behavior).ok() == Some(DropBehavior::DropCascade) {
    return Err(ParseError::ParseError(
        "DROP ... CASCADE is not supported; dependent objects must be dropped explicitly"
            .into(),
    ));
}
```

This fires before any object-type dispatch, so `DROP TABLE ... CASCADE`, `DROP
VIEW ... CASCADE`, `DROP INDEX ... CASCADE`, `DROP TYPE ... CASCADE`, `DROP
DOMAIN ... CASCADE` all hard-error today (there is a test pinning this,
`translator.rs:7362`, `test_drop_cascade_rejected_not_silently_ignored`). That is
the right interim choice per this project's "reject unsupported syntax with a
clear error over silently wrong results" rule, but it means any PG application
or migration script that uses `CASCADE` (a common pattern for dropping a table
and its dependent views/constraints in one statement) cannot run against
pgmicro at all today.

**`DROP SCHEMA` is the one exception** and already has bespoke, working
RESTRICT/CASCADE logic — `core/pg_dispatch.rs:242-262` (`drop_one_schema`):
non-cascade drop of a non-empty schema errors ("cannot drop schema ... because
other objects depend on it"); cascade drop presumably proceeds to drop the
schema's tables (verify the cascade branch's continuation past line 262 when
implementing — not fully re-read here). This is useful precedent for message
wording, but it is a **schema-container-scoped** cascade (walks "tables inside
this schema"), not PostgreSQL's actual **object-dependency-graph** cascade
(walks "everything that references this specific object," which crosses
schemas freely — e.g. a view in schema B selecting from a table in schema A).
Do not generalize the schema-drop code path directly; it solves a narrower
problem than what `DROP TABLE`/`DROP VIEW ... CASCADE` need.

### 2. There is no dependency tracking for the two things that matter most: views-on-tables and FK-constraints-on-tables

`validate_drop_table` (`core/translate/schema.rs:1734-1754`) is the entire
pre-drop validation for `DROP TABLE`. It checks exactly two things: system-table
protection, and "is this actually a materialized view" (must use `DROP VIEW`
instead). **It does not check whether any regular view's query references this
table.** Confirmed by reading the function in full — there is no call into
anything view-related. This means, **today, `DROP TABLE t` where `CREATE VIEW v
AS SELECT * FROM t` exists succeeds unconditionally**, leaving `v` behind as a
view over a now-nonexistent table. Real PostgreSQL refuses this by default:

```
ERROR:  cannot drop table t because other objects depend on it
DETAIL:  view v depends on table t
HINT:  Use DROP ... CASCADE to drop the dependent objects too.
```

This is a correctness gap independent of whether `CASCADE` support ever ships —
pgmicro currently silently permits an operation real PG rejects, and leaves a
dangling/broken view rather than erroring. Fixing the RESTRICT-by-default check
is arguably higher priority than CASCADE itself, since it changes an operation
from silently-wrong to correctly-rejected.

Materialized views are the one exception with partial infrastructure:
`Schema::table_to_materialized_views` / `add_materialized_view_dependency` /
`get_dependent_materialized_views` (`core/schema.rs:645,1049,1060`) track which
materialized views read from which base tables — but this map is populated only
for materialized views (`translate/schema.rs:1692`, at `CREATE MATERIALIZED
VIEW` time) and, from what's visible at the registration site, is consumed for
materialized-view invalidation/refresh bookkeeping, not for blocking `DROP
TABLE`. **Verify at implementation time** whether `DROP TABLE` on a table with
dependent materialized views currently errors, silently succeeds, or partially
works — the two-search above did not turn up a call site wiring this map into
`validate_drop_table`, meaning materialized views likely have the same gap as
regular views. Confirm before assuming plain views are the only affected case.

FK constraints have tracking (`Schema::any_resolved_fks_referencing`,
`resolved_fks_referencing`, `core/schema.rs:2109` and neighboring), but it feeds
**the wrong mechanism for this problem**: `emit_fk_drop_table_check`
(`core/translate/fkeys.rs:1969-2006`) fires the constraint's `ON DELETE` action
(`CASCADE`/`SET NULL`/`SET DEFAULT`: run the row-level action against every row
in the referencing table; `RESTRICT`/`NO ACTION`: count violations) — this is
**`DELETE FROM parent` row-cascade semantics**, correct for deleting rows, but
**not what real PostgreSQL does for `DROP TABLE parent`**. Real PG's `DROP
TABLE` with an FK-referencing child table does not touch the child table's
*rows* at all regardless of the constraint's `ON DELETE` action — it treats the
FK *constraint itself* as a dependent object: RESTRICT (default) errors
("constraint fk_name on table child depends on table parent"); CASCADE drops
the FK constraint (and only the constraint — the child table and its rows are
untouched). **This needs verification against live `psql`, not assumption** —
but if confirmed, it means today's `emit_fk_drop_table_check` path is
answering a different question (row-cascade eligibility) than the one `DROP
TABLE` should be asking (catalog-dependency eligibility), and reusing it as-is
for CASCADE support would be a semantic bug, not a feature gap. Do not wire
`DROP TABLE ... CASCADE` to trigger `ON DELETE CASCADE` row actions — confirm
with `psql` exactly what CASCADE drops (constraint only) versus what plain
`DROP TABLE parent` does to an FK-referenced child's rows (nothing) before
touching this code path.

## Design

Add a minimal, generic object-dependency registry to `Schema` — "what depends
on object X" — queried by every `DROP` path (table, view, materialized view,
type, domain — index/trigger dependency semantics are narrower in real PG and
lower priority, see Out of scope) before the drop proceeds:

- **Dependency edges to track, in priority order** (highest-value first, verify
  each edge kind's exact PG wording before implementing):
  1. View → base table/view (a view's `FROM`/JOIN references).
  2. FK constraint → referenced table (already tracked via
     `resolved_fks_referencing`; reuse the tracking, but gate a *new* check —
     "does dropping this table require dropping a constraint" — rather than
     routing through `emit_fk_drop_table_check`'s row-action machinery).
  3. Materialized view → base table (extend the existing
     `table_to_materialized_views` map's *consumers* — the map itself may
     already be sufficient; verify whether it needs a new read path or already
     has one that's simply not wired into `validate_drop_table`).
- **RESTRICT (no `CASCADE` keyword) is PostgreSQL's actual default** for every
  object type covered here — verify this is really documented as the default
  before relying on it (it is, per `DROP TABLE`'s reference page, but confirm
  the exact behavior isn't RESTRICT-only-for-some-object-kinds). When any
  dependency edge exists and `CASCADE` was not requested: error, do not drop.
  Exact error/DETAIL/HINT wording must match a live `psql` check — this
  document's wording above is a memory transcription, not a verified quote.
- **`CASCADE`**: recursively walk the dependency graph from the dropped object,
  drop every dependent object (deepest first — a view depending on a view
  depending on the table needs the outer view dropped before the inner), and
  drop each one's own dependents in turn. FK constraints are dropped as
  constraints, not by dropping the referencing table.
- **NOTICE-per-dropped-object**: real `psql` prints one `NOTICE: drop cascades
  to view v1` (or similar; verify exact wording) line per object cascade-caught.
  Turso's engine has no existing "notice" channel independent of `Result`
  errors — **investigate whether one is needed/exists** (grep for any
  warning/notice plumbing in `core/` before assuming none exists) as part of
  this plan, since without it pgmicro's wire server (`cli/pg_server.rs`) has
  nothing to forward as a PG protocol `NoticeResponse`. If no such channel
  exists, decide (and document the decision, don't silently skip) whether
  emitting the NOTICE is in scope for this plan or is cleanly deferrable to the
  pgmicro-side follow-up (the follow-up plan already owns wire-protocol-level
  concerns per the two-plan split).

### Where this plugs into existing DROP paths

- `core/translate/schema.rs::validate_drop_table` — add the dependency check
  here (currently only system-table + materialized-view checks).
- The equivalent view-drop and type/domain-drop translate functions (locate via
  `rg -n "fn translate_drop_view|fn translate_drop_type|fn translate_drop_domain"
  core/translate/schema.rs` — not enumerated here since this document did not
  read those specific functions; do so before implementing) need the same
  dependency-check call.
- `core/schema.rs` — add the new dependency-edge storage (a
  `HashMap<String, Vec<DependentObject>>` keyed by depended-on object name,
  analogous to the existing `table_to_materialized_views` map, but generalized
  across view/FK/matview edge kinds rather than one map per kind — implementer's
  call whether one generalized map or several kind-specific maps is cleaner;
  precedent in this file is kind-specific maps, so matching that convention may
  be preferable per "match the codebase's conventions").

## Explicitly out of scope

- **Index and trigger CASCADE**: PG's dependency rules for these are narrower
  (an index dropped via `DROP INDEX` has no PG concept of "things depending on
  an index" the way tables/views do — verify, but this is very likely a
  non-issue) and the existing hard-rejection is a smaller loss for these object
  types. Leave `DROP INDEX/TRIGGER ... CASCADE` rejected as today unless
  investigation during implementation turns up a real gap.
- **`pg_depend`-as-a-queryable-catalog-table**: this plan only needs an internal
  registry sufficient to answer "what depends on X" for DROP-time checks — it
  does not need to expose a queryable `pg_depend` PG catalog virtual table (that
  would be a separate `core/pg_catalog.rs` addition/pgmicro-side concern, only
  worth doing if a real workload queries `pg_depend` directly).
  RECHECK: cross-reference with the pgmicro follow-up plan for whether
  `pg_depend`-table population is actually needed for `psql`'s own `\d+`/dependency
  introspection commands before ruling it fully out of scope — `psql` internally
  queries `pg_depend` for some display commands, and if pgmicro-compatible `\d+`
  behavior already depends on this, that changes the priority. Flag, don't
  silently assume "no" without checking.
- **Sequence/DEFAULT-expression dependencies** (a sequence used by a column's
  `DEFAULT nextval(...)`): real PG tracks these too (`DROP SEQUENCE` used by a
  column default without CASCADE errors), but this project's sequence support
  is itself a separate, not-yet-implemented two-plan item — do not build
  sequence-dependency edges before sequences exist. Flag as follow-up once the
  Sequences plan lands.
- **Row-level FK cascade changes**: `emit_fk_drop_table_check`'s existing
  `DELETE`-time behavior is untouched by this plan — it is correct for `DELETE
  FROM parent`, just not reusable as-is for `DROP TABLE parent`'s dependency
  check (see Problem #2).

## Testing

- Unit tests in `core/schema.rs` or wherever the new dependency map lives:
  register a view-depends-on-table edge, confirm the lookup returns it; confirm
  an unrelated table has no dependents.
- Integration-level tests (`cargo test -p core_tester` or the relevant crate)
  covering, **each cross-checked against real `psql` output first**:
  ```sql
  CREATE TABLE t (id INTEGER);
  CREATE VIEW v AS SELECT * FROM t;
  DROP TABLE t;              -- expect: ERROR (RESTRICT default, v depends on t)
  DROP TABLE t CASCADE;      -- expect: succeeds, v is also dropped
  SELECT * FROM v;           -- after cascade drop: expect ERROR (v no longer exists)
  ```
  ```sql
  CREATE TABLE parent (id INTEGER PRIMARY KEY);
  CREATE TABLE child (id INTEGER, p_id INTEGER REFERENCES parent(id));
  INSERT INTO child VALUES (1, 1);
  DROP TABLE parent;          -- expect: ERROR (RESTRICT — FK constraint depends on parent)
  DROP TABLE parent CASCADE;  -- expect: succeeds, FK constraint dropped
  SELECT * FROM child;        -- after cascade drop: expect child + its row(s) UNCHANGED, still exist
  ```
  The last assertion is the one this document is least confident about from
  memory — verify with `psql` that cascade-dropping the parent does not touch
  child rows, only the constraint, before trusting this test's expected value.
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` and
  `cargo fmt` per standard workflow.

## Migration / compatibility

Pure behavior extension for `CASCADE`, but a **behavior change** for plain
(non-cascade) `DROP TABLE`/`DROP VIEW` on an object with dependents: today this
silently succeeds; after this plan, it errors (matching real PG's RESTRICT
default). This is a deliberate correctness fix, not a regression, but flag it
prominently in the follow-up pgmicro-side plan/changelog since any existing
pgmicro user relying on today's silent-success behavior will see a new error
on upgrade.
