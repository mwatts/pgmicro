# pgmicro Plan: Wire `CREATE SEQUENCE`, `nextval`/`currval`/`setval`, `GENERATED ... AS IDENTITY`, and real SERIAL backing sequences (Task B2 / Workstream D follow-up)

**Depends on:** `2026-07-03-turso-core-sequences.md` landing first (adds the
`Sequence` engine object — non-transactional `advance()`/`last_advanced_value()`/
`set()`). Do not start this plan before that one merges; every statement below
assumes the primitive already exists. The core plan itself flags the durable
non-transactional write path as genuinely new engineering with no precedent in
`core/storage/` today — expect this dependency to take real time, not a quick
merge.

## Context

Two previously-separate two-plan candidates in `2026-07-02-pgmicro-fixes.md`
are superseded by the single core plan above, because both need the same
backing primitive:

- **Task B2** (search `ConstrIdentity` in the master plan): `GENERATED ... AS
  IDENTITY` currently fails loud —
  `parser_pg/src/translator.rs:371-376` unconditionally returns
  `ParseError::ParseError("GENERATED ... AS IDENTITY is not supported (no Turso
  equivalent to PG sequences); use SERIAL or a manually managed default
  instead")` for `ConstrType::ConstrIdentity`. Verified directly by reading the
  match arm — this is a clean, deliberate rejection (not a silent
  mistranslation), consistent with this project's "reject unsupported syntax
  with a clear error" principle. This plan replaces that rejection with real
  translation once the core primitive exists.
- **The Workstream D "not specced" block** (search `CREATE SEQUENCE` in the
  master plan): `CREATE SEQUENCE`/`nextval`/`currval`/`setval` support.
  Confirmed via grep across `parser_pg/src/translator.rs` that **none** of
  `CreateSeqStmt`, `AlterSeqStmt`, `nextval`, `currval`, or `setval` appear
  anywhere in the file today — there is no partial implementation to build on,
  this is greenfield translator work once the core primitive lands.

## What SERIAL actually does today (verify before assuming this plan is only additive)

`is_serial_type()` (`parser_pg/src/translator.rs:4770`) matches
`SERIAL`/`SERIAL2`/`SERIAL4`/`SERIAL8`/`SMALLSERIAL`/`BIGSERIAL`. When a
`CREATE TABLE` column uses one of these types, the translator (lines 214-222,
395-406):

1. Sets a table-level `has_autoincrement` flag if *any* column is serial.
2. Forces that column to be `PRIMARY KEY` and `NOT NULL`.
3. Maps the PG type to plain SQLite `"INTEGER"` (`map_pg_type`, same base-type
   mapping as `INT4`).

There is **no real backing sequence object created at all** — this is purely
SQLite's `AUTOINCREMENT` on the rowid column (via `sqlite_sequence`, a
per-table row, `core/translate/schema.rs:1267`). Confirmed via grep: no
`<table>_<column>_seq`-style name is generated anywhere, and `pg_catalog.rs`
has zero mentions of "sequence" (checked via `rg -i sequence core/pg_catalog.rs`
— no output). This means today, real PostgreSQL client code that does
`SELECT nextval('orders_id_seq')` or `SELECT currval(pg_get_serial_sequence('orders', 'id'))`
against a pgmicro SERIAL column has **nothing to call** — there is no
`orders_id_seq` object in existence, catalog or otherwise. Fixing SERIAL to
create a real, independently-queryable sequence is in scope for this plan, not
a separate follow-up — it is the same primitive as `GENERATED ... AS IDENTITY`
and explicit `CREATE SEQUENCE`, and PG's own `SERIAL` is documented as pure
syntactic sugar for "create a sequence, then set the column's default to
`nextval('that_sequence')`, then mark the sequence as owned by the column."
**Verify against live `psql`**: confirm `\d tablename` on a table with a
`SERIAL` column really does show `nextval('tablename_id_seq'::regclass)` as
the column default, and that `orders_id_seq` really is independently visible
in `\ds` / `pg_class` — do not implement against a remembered approximation of
this.

## What this plan actually does

1. **`CREATE SEQUENCE` / `ALTER SEQUENCE` / `DROP SEQUENCE` translation.**
   Confirm what protobuf node types `pg_query` produces for these (near-certain
   `CreateSeqStmt`/`AlterSeqStmt`/`DropStmt` with an object-type discriminator,
   since `pg_query` is PostgreSQL's real parser — verify the exact shape by
   parsing a sample statement and inspecting the result, per this project's
   documented debugging workflow, rather than guessing field names). Add a new
   `translate_create_sequence`/`translate_alter_sequence` in
   `parser_pg/src/translator.rs`, mapping PG's `CREATE SEQUENCE name [AS type]
   [INCREMENT BY n] [MINVALUE n] [MAXVALUE n] [START WITH n] [CACHE n] [CYCLE |
   NO CYCLE] [OWNED BY ...]` options onto the core `Sequence` object's
   start/step/bounds/cycle properties from the core plan. `OWNED BY` (PG's
   mechanism for a sequence being dropped automatically when its owning column
   is dropped) needs its own small dependency-tracking hook — check whether the
   cascading-drop follow-up plan's dependency graph (see
   `2026-07-03-turso-core-cascading-drop.md`) is the right place to register
   this, or whether it needs its own simpler mechanism; do not build a second,
   divergent dependency-tracking system if the cascading-drop one already
   covers "object X is dropped when object Y is dropped."

2. **`nextval(regclass)`, `currval(regclass)`, `setval(regclass, bigint
   [, boolean])` as registered scalar functions.** These take a sequence name
   (or OID, via PG's `regclass` pseudo-type) as their first argument, not a
   literal identifier — the translator must resolve the string/OID argument to
   a `Sequence` at *execution* time (looked up from `Schema::sequences` by
   name), not translate-time, since the argument is a runtime expression, not
   fixed syntax (e.g. `nextval(current_setting('my.seq_name'))` is valid real
   PG and cannot be resolved during translation). Register in
   `core/functions/postgres.rs` (per this project's "Adding a PG system
   function" workflow) as functions that call the core plan's
   `Sequence::advance()`/`last_advanced_value()`/`set()`. `currval()`'s
   "connection never called `nextval()` on this sequence in this session"
   case must surface as a real error, not `NULL` — the core plan's Design
   point 3 flags this as needing live-`psql` verification (PG documents this
   as an error: `ERROR: currval of sequence "s" is not yet defined in this
   session`); confirm the exact SQLSTATE/message before wiring the pgmicro
   error path, and map it through `limbo_error_to_pg`
   (`cli/pg_server.rs:1719-1775`) consistent with how every other `LimboError`
   variant is mapped.

3. **`GENERATED ... AS IDENTITY` wiring.** Replace the
   `ConstrType::ConstrIdentity` rejection at
   `parser_pg/src/translator.rs:371-376` with: create an implicit backing
   `Sequence` (PG's naming convention is `<table>_<column>_seq`, per the core
   plan's explicit note that dialect-specific naming is out of scope for it
   and belongs here), then set the column's default expression to
   `nextval('<table>_<column>_seq')` and register the `OWNED BY` relationship
   from step 1. Distinguish `GENERATED ALWAYS AS IDENTITY` (rejects explicit
   `INSERT`/`UPDATE` of the column unless `OVERRIDING SYSTEM VALUE` is given)
   from `GENERATED BY DEFAULT AS IDENTITY` (allows explicit values, same as
   `SERIAL`) — **verify against live `psql`** that this ALWAYS/BY DEFAULT
   distinction is real (it is documented PG behavior) and confirm the exact
   error PG raises for the ALWAYS case before implementing the rejection path.
   There is an existing parser test at `translator.rs:6387` asserting
   `"CREATE TABLE t (id INTEGER GENERATED ALWAYS AS IDENTITY)"` currently
   raises the old rejection error — this test must be rewritten to assert
   successful translation once this plan lands, not left asserting the
   now-obsolete error text.

4. **Real SERIAL backing sequences.** Change SERIAL/BIGSERIAL/SMALLSERIAL
   handling (`is_serial_type`, `translator.rs:4770` and its call sites at
   lines ~214-222 and ~395-406) to create a real named `<table>_<column>_seq`
   `Sequence` (same mechanism as step 3) instead of relying purely on SQLite
   `AUTOINCREMENT`. Whether to keep `AUTOINCREMENT` as an additional
   implementation detail underneath (for rowid allocation performance) or
   replace it entirely with `DEFAULT nextval(...)` is an implementation
   decision — but the **catalog-visible outcome must match real PG**: a
   `SERIAL` column is queryable via `pg_get_serial_sequence('table', 'col')`
   and its backing sequence must appear in `\ds` / `pg_class` /
   `pg_sequences`, which it does not today (see "What SERIAL actually does
   today" above).

5. **Catalog visibility.** `core/pg_catalog.rs` has zero sequence awareness
   today (confirmed via grep — see above). Add: (a) `relkind='S'` rows in
   `pg_class` for each `Schema::sequences` entry (today only `'r'`/`'i'` are
   emitted, per `pg_catalog.rs:378,435`), (b) a populated `pg_sequences` table
   (currently absent — not even listed among this project's stub tables in
   `CLAUDE.md`'s catalog table), and (c) `pg_get_serial_sequence(table, column)`
   as a new function in `core/functions/postgres.rs`, since real client code
   and ORMs commonly call it to discover a SERIAL column's backing sequence
   name.

## Testing

Per this project's Core Principle 6 ("test with the REPL first, psql
second"), primary verification is `cargo run -p pgmicro -- :memory:`:

```sql
CREATE SEQUENCE s START WITH 1 INCREMENT BY 1;
SELECT nextval('s');   -- 1
SELECT nextval('s');   -- 2
SELECT currval('s');   -- 2
SELECT setval('s', 100, false);
SELECT nextval('s');   -- 100
SELECT setval('s', 100, true);
SELECT nextval('s');   -- 101

CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT);
INSERT INTO t (name) VALUES ('a'), ('b');
SELECT pg_get_serial_sequence('t', 'id');  -- 't_id_seq'
SELECT currval('t_id_seq');                -- 2
\ds                                         -- must list t_id_seq and s

CREATE TABLE t2 (id INTEGER GENERATED ALWAYS AS IDENTITY, name TEXT);
INSERT INTO t2 (name) VALUES ('x');         -- ok
INSERT INTO t2 (id, name) VALUES (5, 'y');  -- must ERROR (ALWAYS forbids explicit value)
INSERT INTO t2 OVERRIDING SYSTEM VALUE VALUES (5, 'z'); -- must succeed
```

Also required:
- Rewrite the now-obsolete rejection test at `translator.rs:6387` to assert
  successful translation.
- `cargo test -p turso_parser_pg` and
  `cargo test -p core_tester --test integration_tests integration::postgres`
  for new translator/catalog tests.
- Every SQL example above must be independently re-run against real `psql`
  before this plan is considered validated — this document's transcription
  of PG's exact error text/behavior (currval-before-nextval error,
  ALWAYS-identity override error, `\d`'s default-expression rendering) is a
  best-effort reconstruction, not a substitute for checking the real server,
  per this project's acceptance bar for "done."

## Out of scope

- Everything the core plan (`2026-07-03-turso-core-sequences.md`) already
  excludes: performance tuning of crash-durability batching, distributed
  sequence behavior.
- `ALTER SEQUENCE ... RESTART` interaction with `TRUNCATE ... RESTART
  IDENTITY` — the master plan already flags `RESTART IDENTITY` as its own
  follow-up (search "RESTART IDENTITY" in `2026-07-02-pgmicro-fixes.md`, notes
  it needs `translate_truncate` to return `Vec<ast::Stmt>` instead of a single
  `ast::Stmt`); this plan should make the underlying `Sequence::set()` call
  available for that follow-up to use, but wiring `TRUNCATE` itself is not
  this plan's job.
- Sequence `GRANT`/`REVOKE`/ownership permission semantics — pgmicro has no
  general privilege system today; out of scope until one exists.

## Open questions flagged for whoever picks this up

- Whether `OWNED BY` dependency tracking should reuse the object-dependency
  graph from `2026-07-03-turso-core-cascading-drop.md` or needs its own
  simpler mechanism — not resolved here, needs a decision once both plans are
  further along.
- The exact wire format / value type `pg_get_serial_sequence` should return
  (plain `TEXT` vs. something `regclass`-typed) — check how pgmicro already
  handles other `regclass`-returning functions, if any exist, before inventing
  a new convention.
