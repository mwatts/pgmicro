# pgmicro Plan: Wire dialect-aware identifier case folding (H34b follow-up)

**Depends on:** `2026-07-03-turso-core-identifier-case.md` landing first, AND
that plan's open decision between approach (a) (dialect-parameterized
`normalize_ident`) and approach (b) (a parallel `fold_delimited_ident` path)
being resolved. This is a joint blocker, not just a sequencing note: the two
approaches wire into pgmicro-visible code differently (one call-site edit per
site vs. a dispatch condition per site), so this plan cannot be written
call-site-by-call-site until that choice is made. Do not start before both are
settled.

## Which approach this plan assumes, and why

This plan assumes **approach (a)** — dialect-parameterized folding via the
existing `Connection::get_sql_dialect()` — is the one chosen, based on
evidence gathered while writing this doc that the core plan's own hedge
("verify a getter exists or add one") resolves in (a)'s favor:
`Connection::get_sql_dialect()` **already exists** (`core/connection.rs:660`,
`798-799`, `809`, `844-845`, `1147`, `1154`, `1187` — used pervasively today
to dialect-branch parsing and pragma behavior), so the per-connection dialect
value approach (a) needs is already threaded and cheap to read at any of the
23 call sites — no new plumbing required to obtain it, only to consult it
inside `normalize_ident`/its callers.

**This is a recommendation, not a decision already made.** The core plan
explicitly reserves this choice for a human, weighing (a)'s "one shared
pipeline" consistency against (b)'s smaller/lower-risk diff. If (b) is chosen
instead, this plan's call-site-level content below (which assumes a single
dialect-aware `normalize_ident`) needs rewriting around a parallel
`fold_delimited_ident` dispatch per site instead — flag that rewrite as
necessary before executing this plan if (b) wins.

## What this plan actually does

### 1. Confirm the "PG-reachable call site" framing doesn't create a partition to exploit

Investigated whether some subset of the core plan's 23 `normalize_ident()`
call sites (`core/connection.rs`, `core/stats.rs`, `core/schema.rs`,
`core/translate/{view,attach,alter,insert,select,delete,analyze,pragma,index,
schema,upsert,trigger,expr,update,planner,trigger_exec}.rs`,
`core/vdbe/execute.rs`, `core/mvcc/database/mod.rs`, `core/function.rs`) are
SQLite-only and could be left untouched. They are not separable this way:
PostgreSQL SQL and SQLite SQL both compile down to the identical
`turso_parser::ast::Stmt` type and flow through this exact same
`core/translate/*.rs` pipeline — pgmicro's whole design principle is "never
re-serialize as SQLite text, translate directly to Turso's AST" (per this
project's own architecture doc). There is no PG-only subset of these files;
every one of them processes PG-dialect-originated ASTs whenever the
connection's dialect is Postgres. Once approach (a) lands, the correctness
condition is per-call, gated on `get_sql_dialect()`'s runtime value, not
per-file. Do not attempt to scope this pgmicro follow-up down to "just the
PG-relevant files" — there isn't a smaller set to find.

### 2. Fix `parser_pg/src/translator.rs`'s own, separate lowercase-folding of schema names

The core plan's grep was scoped to `core/` and found 23 call sites into
`core/util.rs`'s `normalize_ident()`. It did not (and could not, since it's a
Turso-core plan with no Postgres framing) search `parser_pg/`. Verified by
grep: `parser_pg/src/translator.rs` independently lowercases schema names at
five call sites — `.schemaname.to_lowercase()` at lines 66, 5514, 5570, 5654,
5728 (used to route `CREATE SCHEMA`/schema-qualified-table resolution to
ATTACH'd databases, per `try_prepare_pg()`'s "Schema-qualified names resolved
via ATTACH'd databases" behavior) plus a general PG-type-name lowercase at
line 4875. These are entirely translator-owned, independent of
`normalize_ident`, and out of the core plan's scope by construction. This
pgmicro-side plan must audit and fix all of them to respect quoting once the
core primitive lands — schema name case folding (`CREATE SCHEMA "MySchema"`
then `SELECT * FROM "MySchema".t`) is exactly the kind of identifier this
project's own architecture routes through ATTACH, so getting it wrong here
would silently break schema resolution for quoted schema names even after
the core fix lands everywhere else.

### 3. Fix `core/pg_catalog.rs`'s lowercase-keyed lookup tables

`core/pg_catalog.rs:149`'s own doc comment states its internal
`schema.tables` lookup keys are "already lowercased by `normalize_ident`" —
i.e. `pg_class`/`pg_attribute`/etc. virtual-table population code assumes
every stored table name is already folded lowercase and keys its maps
accordingly. Once quoted, case-preserved identifiers exist in `Schema`, this
assumption breaks: a table created as `"MyTable"` would need `pg_class` to
show `relname = 'MyTable'` (exact case, matching real PostgreSQL's
`pg_class.relname`), and any internal map still keying by
`to_lowercase()`-transformed strings would either miss it or collide it with
a same-named lowercase table. Audit every internal map/lookup in
`core/pg_catalog.rs` built from `Schema` table/column names for this same
buried assumption (not just the one instance the doc comment flags — treat
that comment as a signal there are likely more, not an exhaustive list) and
fix each to preserve exact-case storage names.

### 4. Fix `core/pg_catalog.rs:3844-3851`'s independent type-name lowercasing

Separately from the identifier-storage issue above, `core/pg_catalog.rs`
lines 3844-3851 lowercase *type names* (not object identifiers) when parsing
type modifiers (e.g. `numeric(10,2)` variants) — verify this is purely a
type-name-keyword-matching concern (PG type names are themselves
case-insensitive keywords, distinct from user-chosen identifiers) and
therefore correctly out of scope for this plan; do not touch it unless
investigation shows it's actually resolving a user-supplied identifier rather
than a fixed PG type keyword.

### 5. End-to-end test matrix (REPL first, psql second — Core Principle 6)

Run via `cargo run -p pgmicro -- :memory:` first, confirm via `psql` against
real PostgreSQL second:

```sql
-- Unquoted folding (unchanged behavior)
CREATE TABLE Foo (x int);
SELECT * FROM foo;    -- must match
SELECT * FROM FOO;    -- must match
SELECT * FROM Foo;    -- must match

-- Quoted, case-sensitive (new behavior)
CREATE TABLE "Bar" (y int);
SELECT * FROM bar;     -- must NOT match (real PG: relation "bar" does not exist)
SELECT * FROM "Bar";   -- must match

-- Cross-form collision (unquoted folds to the same stored string)
CREATE TABLE "foo2" (z int);
CREATE TABLE foo2 (w int);  -- must ERROR: relation "foo2" already exists

-- Column-level (catches the "inconsistent across kinds" trap the core plan warns about)
CREATE TABLE "Baz" ("Qux" int);
SELECT "Qux" FROM "Baz";   -- must match
SELECT qux FROM "Baz";     -- must NOT match (real PG: column "qux" does not exist)

-- Schema-qualified, exercising this plan's item 2 fix specifically
CREATE SCHEMA "MySchema";
CREATE TABLE "MySchema".t (a int);
SELECT * FROM "MySchema".t;  -- must match
SELECT * FROM myschema.t;    -- must NOT match (real PG: schema "myschema" does not exist)

-- Catalog visibility, exercising this plan's item 3 fix specifically
CREATE TABLE "CaseSensitive" (a int);
SELECT relname FROM pg_class WHERE relname = 'CaseSensitive';  -- must return the row
```

Add these as new tests in `pgmicro/tests/pgmicro.rs` (REPL-level, stdin/stdout)
and `tests/integration/postgres/` (Rust API level).

### 6. Reconcile with the two already-shipped, narrower interim tasks

- **`normalize_role_name`** (`core/pg_role.rs:206-229`, master plan line
  ~1446-1448) is a **separate, already-fixed, self-contained bug** — it had
  its own local quote-strip-then-unconditional-lowercase pattern, unrelated
  to the shared `normalize_ident()`/`Name`-equality path this plan targets.
  Nothing to reconcile; it does not need revisiting once this plan lands
  (confirmed by the original quality-review finding text itself: "no
  two-plan-rule follow-up needed here").
- **Task F2** (`pgmicro/src/main.rs`'s `\d`/`\d+` quote-stripping via
  `unquote_identifier()`, master plan line ~7413-7528) **stays useful and
  correct after this plan lands** — it strips/unescapes the meta-command
  argument's quote syntax and folds unquoted args to lowercase before doing
  a lookup; that behavior is exactly what a quoted-identifier-aware
  `\d "Foo"` needs to look up the exact-case stored name `Foo`, and exactly
  what an unquoted `\d foo` needs to look up the folded name `foo`. F2's own
  code comment already states it "matches PostgreSQL quoted-identifier
  rules" — it was written anticipating this fix, not against today's
  always-lowercase behavior. No changes needed to F2 once this plan lands.
- **Task F3** (`core/translate/schema.rs:1158-1174`, master plan line
  ~7532-7554) **becomes dead code / unreachable once this plan lands** for
  Postgres-dialect connections: F3 makes the error message honest about
  *why* a case-insensitive collision happened, but once collisions of that
  kind become impossible by construction (unquoted-vs-quoted no longer
  collide unless they resolve to the same stored string), the code path
  triggering F3's message can only fire for genuine same-stored-string
  collisions (already correctly an error in both old and new behavior) or
  for SQLite-dialect connections (where case-insensitive collision remains
  intentional, not an error). Do not delete F3 as part of this plan —
  confirm via a regression test that it still fires correctly for SQLite
  dialect, and downgrade/remove its Postgres-dialect-specific messaging only
  if investigation confirms it is provably unreachable there.

## Testing

- `cargo test -p pgmicro` (new REPL tests from item 5)
- `cargo test -p core_tester --test integration_tests integration::postgres`
  (new Rust API tests from item 5, including the `pg_catalog` visibility case)
- `cargo test -p turso_parser_pg` (regression — confirm the five
  `to_lowercase()` sites in translator.rs behave identically for
  SQLite-dialect-adjacent paths, if any, and correctly for the new
  quoted-schema case)
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`

## Out of scope

- Everything the Turso-core plan already excludes: no SQLite-dialect
  behavior change, no storage-format/migration work beyond what that plan
  already specifies as unnecessary.
- Task F2 and Task F3 themselves (see item 6) — not touched by this plan,
  only assessed for continued relevance.
- `\d`/`\d+` meta-command quoting beyond confirming F2 still functions
  correctly (Task F2/H34a is independently scoped and already shipped).
- Rewriting this plan around approach (b) — only needed if the core plan's
  human decision goes the other way (see "Which approach" section above).
