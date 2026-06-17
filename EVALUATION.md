# pgmicro — Code Evaluation

In-process PostgreSQL front-end on Turso's SQLite-compatible engine. PG SQL → libpg_query
(C FFI, PostgreSQL's real parser) → protobuf → translator → Turso AST → shared SQLite
compile/exec pipeline. Plus PG catalog virtual tables, system functions, a wire-protocol
server, and a REPL with psql meta-commands.

## Does it work?

Yes, for core OLTP. ~400+ tests, exercised end-to-end (binary stdin/stdout, Connection API,
wire TCP). Real coverage of SELECT/INSERT/UPDATE/DELETE, DDL, CTEs, UNION/INTERSECT/EXCEPT
(including ALL), window functions, subqueries, arrays, schemas, catalog tables, COPY FROM.
Not a demo — the actual execution path is sound. The architecture (translate AST directly,
never re-serialize SQL) is the right call.

## What it actually does

| Layer | Real capability |
|---|---|
| Translator | Full DML, most DDL, ~all common expressions/operators, JSON `->`/`->>`, ILIKE, SIMILAR TO, BETWEEN, IS DISTINCT, CASE, COALESCE, casts, ARRAY, window frames, RETURNING, ON CONFLICT, GREATEST/LEAST, DISTINCT ON, INTERVAL/MONEY |
| Catalog | `pg_class/attribute/namespace/type/index/constraint/attrdef/proc/tables` populated from live schema; ~12 stubs for psql compat |
| Wire | Simple + Extended query protocol, multi-statement, trust auth (localhost-only; no TLS) |
| REPL | 19 `\` meta-commands, all tested |
| Schemas | CREATE/DROP SCHEMA → ATTACH separate `.db` files; `search_path` drives unqualified resolution |

## Gaps — by severity

### Security

**Fixed (localhost wire server):**

- **Path traversal** — `validate_schema_name()` in `core/pg_schema.rs` rejects schema names
  outside `[A-Za-z0-9_]` at CREATE/DROP SCHEMA time (before `turso-postgres-schema-<name>.db`
  paths are built). Defense-in-depth check in `cli/pg_server.rs` cleanup. Stricter than
  PostgreSQL identifier rules (no dots, Unicode, etc.); documented in `pg_schema.rs`.
- **Delete-before-execute** — wire server deletes schema `.db` files only after successful
  execution, not after `prepare`. Tested via API and wire protocol.
- **Prepared DROP SCHEMA cleanup** — extended-protocol drops resolve `$N` schema placeholders
  from portal parameters before deleting `turso-postgres-schema-<name>.db` files. Literal
  prepared `DROP SCHEMA name` also triggers cleanup on the extended path.

**Accepted for localhost-only use:**

- **No auth / no TLS** — `NoopHandler` still returns `AuthenticationOk` to any client that
  can reach the port. CLI help and startup stderr warn to bind `127.0.0.1` only. Intended for
  trusted local development, not public network exposure.

### Correctness — wrong results, silent

**Fixed:**

- **`NOT IN (subquery)` / `<> ALL (subquery)`** — `AllSublink` and `InSelect` now set
  `not: true`; `NOT` over `IN` subquery/list is folded correctly.
- **NULLS FIRST/LAST in ORDER BY** — `translate_sort_nulls()` maps PG defaults and explicit
  `NULLS FIRST`/`LAST` to Turso `NullsOrder`.
- **DISTINCT ON** — `row_number()` rewrite (`wrap_distinct_on()`). Requires `ORDER BY`; not
  supported on VALUES or compound SELECT.
- **INTERSECT ALL / EXCEPT ALL** — rewritten via `row_number()` / `COUNT()` subqueries
  (`fold_intersect_all`, `fold_except_all`); preserves duplicate multiplicity.
- **GREATEST / LEAST** — dedicated variadic `greatest`/`least` scalars in
  `core/functions/postgres.rs` (PG NULL semantics: any NULL arg → NULL).
- **Multi-table TRUNCATE** — `try_prepare_pg` intercepts multi-relation `TRUNCATE` and runs
  `DELETE FROM` on each table sequentially.
- **Multi-command ALTER TABLE** — `translate_stmts()` emits one `AlterTable` per command;
  `compile_and_run_cmds()` executes all.
- **`= ANY(array)`** — `expr = ANY(array)` → `array_contains(array, expr)`; subquery form
  still uses existing `SubLink` path.
- **`IS TRUE` / `IS FALSE`** — maps to `Literal::True`/`False` so `Insn::IsTrue` applies PG
  truth semantics (any non-zero number is true).
- **`gcd` / `lcm` overflow** — return `LimboError::IntegerOverflow` (SQLSTATE 22003) instead
  of a TEXT `"ERROR: ..."` row value.
- **Aggregate ORDER BY** — `FuncCall.agg_order` is translated into `FunctionCall.order_by`;
  planner stores `order_by` on `Aggregate` and emits a sorter path (ungrouped: dedicated
  `AggOrderMetadata` sorter; grouped: extra sort keys on the GROUP BY sorter) so `AggStep`
  runs in sorted order. `array_agg(x ORDER BY y)` works end-to-end.
- **INTERVAL / MONEY types** — Turso builtin `interval` (16-byte blob, calendar semantics) and
  `money` (int64 cents) in core; pgmicro maps DDL/casts, rewrites `timestamp ± interval` and
  `EXTRACT(... FROM interval)`, and wires catalog OIDs (1186/790) plus wire
  `Type::INTERVAL`/`Type::MONEY`. See `docs/design/interval-money-types.md`.
- **Ungrouped `COUNT(*)` with custom-type WHERE** — inline AggFinal result path when WHERE
  constant-folds before the main loop; wrapped aggregates like `COALESCE(sum(v), 0)` use the
  normal epilogue (not bare-aggregate inline copy).

### Wire protocol fidelity

**Partially fixed:**

- **SQLSTATE codes** — `cli/pg_server.rs` maps `LimboError` variants and message patterns to
  PostgreSQL SQLSTATE (e.g. `42601` syntax, `42P01` undefined_table, `23505` unique_violation,
  `40001` serialization_failure, `22P02` invalid_parameter). Unclassified errors still use `XX000`.

**Remaining:**

- **Binary param format unsupported**, `cli/pg_server.rs:441`. Binary bytes decoded as UTF-8
  → garbage/error. JDBC/psycopg3 binary mode breaks.
- **NUMERIC = f64** everywhere. Precision silently lost.
- **No row streaming**, `max_rows` ignored. Full result set buffered in memory.
- **No SSL, no cancellation, no COPY-over-wire, no LISTEN/NOTIFY.**
- **Type OID vs value mismatch**: INT4 column emits i64-encoded value; bare `NUMERIC`
  advertised as FLOAT8 but `NUMERIC(10,2)` as NUMERIC — inconsistent.

### Catalog faithfulness

**Remaining:**

- **Single hardcoded `turso` role** everywhere (`pg_roles`, `tableowner`,
  `pg_get_userbyid`). Multi-user joins all wrong.
- **`pg_database` = 1 row.** `\l` never shows attached schemas.
- **No `pg_authid`/`pg_user`/`pg_enum`/`pg_description`/`pg_collation`** populated. `\du`,
  `\dT+` error or empty.
- **Enum OID = poly hash mod 10000**, `core/pg_catalog.rs:1887`. ~130 enums → >50% collision
  chance. OID space collides with `pg_attrdef` (both 50000+).
- **`pg_proc` OIDs ephemeral** — reassigned 1..n per scan, not stable.
- **`atttypmod` always -1** — clients can't recover varchar/numeric length.
- **Every vtab does full scan** — `argv_index: None`, no OID fast path. O(all attributes) per
  catalog query.

### Dialect / schema mechanism

**Fixed:**

- **`SET`/`SHOW`/`RESET`/`RESET ALL` for `search_path`** — stored on `Connection.pg_search_path`;
  `SHOW`/`RESET` work. Other `RESET name` is a no-op.
- **Unqualified name resolution** — `Resolver` walks `search_path` order in Postgres mode
  (`public` → main DB, schema names → attached DBs).
- **`SET LOCAL search_path`** — transaction-scoped; restored on `COMMIT`/`ROLLBACK` via
  `pg_search_path_local_saved` stack.
- **Connect-time dialect** — new connections default to SQLite (`SqlDialect::default()`).
  Postgres entry points set dialect explicitly (pgmicro REPL, PG wire server, NAPI
  `default-postgres`). Avoids Cargo feature-unification from pgmicro breaking `core_tester`
  SQLite tests when built in the same invocation.
- **`prepare_execute_batch`** — SQLite path loops `Parser::next_cmd()` (matches `execute()`);
  PG path runs all statements from `translate_stmts()`.

**Remaining:**

- **`SET LOCAL` for other GUCs** — only `search_path` has txn-local restore; other `SET LOCAL`
  still leaks to session scope.
- **Cross-schema txns not atomic** — separate ATTACH WAL files, partial commit possible,
  undocumented.
- **`:memory:` + CREATE SCHEMA** writes a physical file to cwd; sessions collide.
- **Multi-statement `prepare()` return value** — when `prepare()` sees multiple commands it
  executes all via `compile_and_run_cmds()` but returns a dummy `Statement` (`SELECT 0 WHERE 0`).
  Use `prepare_execute_batch()`, `execute()`, or wire simple protocol for intentional batching.

### Validation bugs

**Remaining:**

- Date validation accepts Feb 31 / Apr 31, `core/pg_catalog.rs:2958`.
- `convert_to_postgres_ddl` naive string replace can rename columns containing `INTEGER`,
  `core/pg_catalog.rs:3427`.
- JSON validation checks bracket balance only, not grammar — `{"a":}` passes.

### Dead code

- `parser_pg/src/{ast,lexer,parser,token}.rs` (~5k LOC hand-written PG parser) **unused** —
  execution uses libpg_query. AGENTS.md confirms.
- `information_schema.tables → sqlite_master` map (`translator.rs:81`) is **dead** — schema
  prefix is stripped before it's reached.

## Next wave (queued)

Work in priority order; each item = branch off `pgmicro-fixes` → PR → squash merge.

| # | Branch | Scope | Notes |
|---|--------|-------|-------|
| 1 | `fix/wire-binary-params` | `cli/pg_server.rs` | Binary portal param format (JDBC/psycopg3) |
| 2 | `fix/pg-database-schemas` | `core/pg_catalog.rs` | `pg_database` rows for ATTACH'd schemas; `\l` |
| 3 | `fix/catalog-atttypmod` | `core/pg_catalog.rs` | `atttypmod` for varchar/numeric precision |
| 4 | `fix/pg-catalog-validation` | `core/pg_catalog.rs` | Date, DDL string-replace, JSON validation |

**Completed on `pgmicro-fixes`:** `fix/interval-money-types` (core types + translator + catalog/wire).

## Bottom line

Core engine works and the design is clean. The major silent-correctness cluster from the
original eval (NOT IN, NULLS, DISTINCT ON, set-op ALL, GREATEST/LEAST, ANY(array), TRUNCATE,
ALTER, IS TRUE, search_path, INTERVAL/MONEY, aggregate ORDER BY) is fixed.

Remaining gaps cluster in two buckets:

1. **PG fidelity for tooling** — binary wire params, catalog stubs (`pg_database`, roles,
   `atttypmod`, enum OIDs), NUMERIC precision. ORMs and typed drivers misbehave; psql mostly
   survives.
2. **Wire server hardening** — auth/TLS if ever exposed beyond localhost; row streaming;
   COPY/LISTEN/NOTIFY.

Strongest part: translator breadth + test depth + rapid correctness fixes on `pgmicro-fixes`.
Weakest: wire-protocol tooling fidelity and catalog completeness for ORM/framework startup.