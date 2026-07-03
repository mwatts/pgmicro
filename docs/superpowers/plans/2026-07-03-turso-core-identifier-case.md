# Turso-Core Plan: Quote-aware identifier case folding

**Status:** not started. Self-contained Turso-core feature — the SQL standard's
delimited-identifier case-sensitivity rule (unquoted identifiers fold to one
case, quoted/delimited identifiers preserve exact case and become
case-sensitive) is not vendor-specific; it is not a Postgres-only concept.

## Problem

`normalize_ident()` (`core/util.rs:120-124`) unconditionally lowercases every
identifier with no concept of whether it was quoted in the original SQL text:

```rust
pub fn normalize_ident(identifier: &str) -> String {
    // quotes normalization already happened in the parser layer (see Name ast node implementation)
    // so, we only need to convert identifier string to lowercase
    identifier.to_lowercase()
}
```

Its own comment is revealing: by the time this function runs, the identifier
has already had its quote *characters* stripped by an earlier layer — but the
*fact that it was quoted* was not carried forward, only the bare text. So
`Foo`, `"Foo"`, and `"FOO"` all normalize to `foo` today: three tables that a
delimited-identifier-aware engine would treat as three distinct objects
collide into one, unrecoverably, at every layer that calls this function
(confirmed by grep: `core/connection.rs`, `core/stats.rs`, `core/schema.rs`,
`core/translate/{view,attach,alter,insert,select,delete,analyze,pragma,index,
schema,upsert,trigger,expr,update,planner,trigger_exec}.rs`,
`core/vdbe/execute.rs`, `core/mvcc/database/mod.rs`, `core/function.rs` —
23 files, every kind of identifier: tables, columns, indexes, views, triggers,
schemas, functions, pragmas).

## Root cause is deeper than `normalize_ident` alone

This is not a one-function fix. Tracing where quote information is lost:

1. **The parser's `Name` AST node (`parser/src/ast.rs:1149-1240`) already
   records quoting** — `Name { quote: Option<char>, value: String }`. Parsing a
   quoted identifier already produces `quote: Some('"')`; an unquoted one
   produces `quote: None`. So the raw information exists at parse time.
2. **But `Name`'s own `PartialEq`/`Hash` deliberately ignore `quote`** (`ast.rs:1153-1163`,
   with an explicit doc comment: "Two Names are equal if they refer to the
   same identifier, regardless of quoting style"). This is intentional and
   correct for real SQLite semantics — SQLite does not fold identifier case at
   all; quoting there only escapes special characters/keywords, and identifier
   matching is ASCII case-insensitive independent of quoting. So the AST layer
   itself already treats quote-vs-unquote as identity-irrelevant, which is
   right for the SQLite dialect and wrong for a dialect that wants delimited
   identifiers.
3. **Every one of the 23 call sites above calls `normalize_ident(&str)` with
   a bare string** (`name.as_str()`, or a `String` already extracted from a
   `Name`), not the structured `Name` value — so even if `Name` carried the
   quote bit faithfully to every call site, `normalize_ident`'s signature
   can't see it. The information is discarded twice: once by `Name`'s
   equality/hash impl, and again by every caller converting to `&str` before
   normalizing.

**Net effect:** fixing this only inside `normalize_ident` is impossible — the
quote bit is already gone well before that function runs, for every code path
in this list. A real fix has to preserve quote-ness from the parser through to
wherever identifier identity is finally decided (schema table/column/index
lookup, primarily `core/schema.rs`'s registries and the DML/DDL identifier
resolution in `core/translate/*.rs`).

## The dialect conflict this design must resolve

SQLite-dialect semantics (must not regress — "SQLite compatibility" is this
project's #2 core principle) and delimited-identifier semantics (needed for
strict SQL-standard/Postgres-family case handling) are **incompatible defaults**:

| | SQLite dialect (today, must stay this way) | Delimited-identifier semantics (needed) |
|---|---|---|
| `Foo` (unquoted) | case-insensitive match, any case | folds to one canonical case (lowercase), matches only that case |
| `"Foo"` (quoted) | same as unquoted — quoting doesn't affect matching | exact-case, case-sensitive, distinct from `foo`/`FOO` |

Because `normalize_ident` and `Name` equality are shared, dialect-agnostic code
paths, this plan must pick one of two approaches — **implementer/reviewer must
choose deliberately, not default to whichever is easier**:

- **(a) Dialect-parameterized identifier folding:** thread `SqlDialect`
  (`core/lib.rs:358`, already available per-`Connection` via
  `Connection::set_sql_dialect`/an equivalent getter — verify a getter exists
  or add one) into identifier resolution, so `normalize_ident` becomes
  dialect-aware and SQLite-dialect connections get today's behavior bit-for-bit
  while a delimited-identifier-mode connection gets the new folding rule. This
  requires threading dialect (or a `Name`, not a bare `&str`) through all 23
  call sites — the larger, more invasive option, but keeps one shared code path.
- **(b) A parallel identifier-resolution path used only by delimited-identifier
  callers:** leave `normalize_ident`/`Name` equality untouched for SQLite, and
  add a new function (e.g. `fold_delimited_ident(name: &Name) -> String`) that
  the delimited-identifier-mode callers invoke instead, at the same 23 sites,
  behind whatever dispatch already distinguishes dialect at each site. Smaller
  diff per call site, but duplicates the call-site list rather than making one
  function dialect-aware.

State a recommendation with evidence before implementation begins — do not
implement both. (a) is more consistent with "one shared Turso pipeline" per
this project's architecture; (b) is more surgical and lower-risk per "minimize
core/ changes." This plan does not resolve that trade-off — it is the first
thing an implementer must decide, with the human, before writing code.

## Folding + comparison rules (verify against live `psql` before trusting this)

Delimited-identifier semantics to replicate exactly, once a
dialect/mode is selected into the new folding path:

- **Unquoted identifier → fold to lowercase**, then compare/store using that
  folded form. `Foo` and `FOO` and `foo`, all unquoted, are the *same* object
  (all resolve to stored name `foo`) — this direction of behavior is
  unchanged from today.
- **Quoted (delimited) identifier → preserve exact original case, no folding**,
  and compare by exact byte equality against other identifiers' resolved forms.
  `"Foo"`, `"FOO"`, and `"foo"` are three distinct objects if all three are
  used to create three separate objects.
- **Cross-form equality:** an unquoted `foo` (folds to `foo`) refers to the
  same object as a quoted `"foo"` (preserved as `foo`) — they collide by
  construction, correctly, because they resolve to the identical stored string.
  An unquoted `foo` never collides with `"Foo"` or `"FOO"` (different stored
  strings after folding/preserving).
- **This must apply uniformly across every identifier kind** — table, column,
  index, view, trigger, schema, function/pragma names — not just tables. A
  fix that only touches `core/schema.rs`'s table registry while
  `core/translate/select.rs`'s column-name matching (`select.rs:1126-1142`)
  still unconditionally lowercases would produce an inconsistent engine where
  `CREATE TABLE "Foo"("Bar" int)` creates the right table but
  `SELECT "Bar" FROM "Foo"` fails to find the column — worse than today's
  uniform-but-wrong behavior, because it would be *inconsistently* wrong.

## Migration / compatibility

This is **additive only if approach (a) is chosen and the SQLite-dialect path
is verified byte-for-byte unchanged**: existing SQLite-dialect databases and
queries must produce identical results before and after, since nothing in
SQLite's own semantics changes. For any *new* delimited-identifier-mode
connection, this changes identifier resolution from "always folds" (today) to
"folds unless quoted" (target) — a database created before this change, under
always-fold behavior, has every stored identifier already lowercased; nothing
about the on-disk representation changes retroactively (stored names are still
just strings), so existing schemas keep resolving exactly as before. The risk
is purely forward: newly-created quoted-case-preserving objects in an old
database are unaffected by anything already there. No storage format change,
no migration script needed — verify this claim with a test that creates
objects under the old build, then opens the same database file under the new
build and confirms lookups still succeed, before treating this as settled.

## Explicitly out of scope

- **`core/pg_role.rs`'s `normalize_role_name`** — already fixed independently
  (see `docs/superpowers/plans/2026-07-02-pgmicro-fixes.md` line ~1446, shipped
  per the progress ledger as "CREATE ROLE preserves case for quoted identifiers
  instead of always folding lowercase"). That was a self-contained,
  single-function bug (the quote-strip-then-unconditional-lowercase pattern
  existed only in that one function, nothing shared). This plan is the
  separate, much larger, shared `normalize_ident()`/`Name`-equality path that
  fix explicitly did not touch.
- **Loud collision errors** (`docs/superpowers/plans/2026-07-02-pgmicro-fixes.md`
  Task F3/H34b) — already scoped as flag-and-document-only against *today's*
  always-lowercase behavior. Once this plan lands, that error condition
  (case-insensitive collision) becomes impossible for delimited-identifier-mode
  connections by construction; F3's message only matters for as long as this
  plan is unimplemented, or for the SQLite dialect where the behavior is
  intentional and not an error at all.
- **`\d`/`\d+` meta-command quoting** (Task F2/H34a) — an independent,
  already-scoped REPL-layer concern, unaffected by which of (a)/(b) above is
  chosen here.

## Testing

- Unit tests in `core/schema.rs`'s test module and wherever `Name`/`normalize_ident`
  already have coverage: unquoted-collision (unchanged), quoted-distinct
  (new), cross-form-collision (new), one test per identifier kind (table,
  column, index, view, trigger) to catch the "uniform across kinds" risk
  called out above.
- `cargo test -p turso_core schema` and the SQLite-dialect regression suite
  (`make test`) run unchanged to confirm zero SQLite-dialect behavior change.
- Manual check via `cargo run -q --bin tursodb -- -q`, cross-checked against
  real `psql` output for the delimited-identifier-mode expectations:
  ```sql
  CREATE TABLE Foo (x int);
  SELECT * FROM foo;   -- matches (unquoted fold)
  CREATE TABLE "Bar" (y int);
  SELECT * FROM bar;   -- must NOT match (quoted, case-sensitive)
  SELECT * FROM "Bar"; -- matches (exact quoted form)
  CREATE TABLE "bar" (z int);  -- distinct object from "Bar" -- must succeed, not collide
  ```
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`.
