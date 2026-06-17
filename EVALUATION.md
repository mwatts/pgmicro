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
| Catalog | `pg_class/attribute/namespace/type/index/constraint/attrdef/proc/tables` populated from live schema; role registry; `pg_description` from COMMENT ON; ~11 stubs for psql compat |
| Wire | Simple + Extended query protocol, multi-statement, lazy row streaming, COPY FROM/TO STDIN/STDOUT (text + binary), optional TLS, query cancellation, LISTEN/NOTIFY pub/sub, trust auth (localhost-only) |
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
- **Role registry in-memory** — `CREATE ROLE` works per connection; not persisted; no password
  verification or `SET ROLE` yet.

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

**Fixed:**

- **SQLSTATE codes** — `cli/pg_server.rs` maps `LimboError` variants and message patterns to
  PostgreSQL SQLSTATE (e.g. `42601` syntax, `42P01` undefined_table, `23505` unique_violation,
  `40001` serialization_failure, `22P02` invalid_parameter). Unclassified errors still use `XX000`.
- **Binary portal parameters** — `pg_bytes_to_value_binary()` decodes int/float/bool/bytea,
  NUMERIC, DATE, TIME, TIMESTAMP/TIMESTAMPTZ, UUID, INTERVAL, and MONEY. Text fallback for
  VARCHAR and unknown types.
- **INT4/INT2 result encoding** — integer columns encode with `i32`/`i16` width when the
  declared PG type is INT4/INT2 (not always i64).
- **NUMERIC precision** — wire binary/text paths preserve decimal text via `BigDecimal`; no
  silent f64 loss on parameters or NUMERIC-typed results.
- **Row streaming** — query results stream row-by-row through pgwire instead of buffering the
  full result set.
- **COPY wire protocol** — `COPY FROM STDIN` / `COPY TO STDOUT` via `CopyHandler` and inline
  copy-out in `cli/pg_server.rs` (text + binary); wire integration tests in `pgmicro/tests/pgmicro.rs`.
- **Optional TLS** — `--tls-cert` / `--tls-key` on pgmicro wire server (`rustls` acceptor).
- **Query cancellation** — cancel request handler maps `(pid, secret)` → `Connection::interrupt()`.
- **LISTEN/NOTIFY pub/sub** — database-scoped hub delivers NOTIFY to all listening sessions;
  wire server pushes `NotificationResponse` asynchronously to connected clients.

**Remaining:**

- **Wire session isolation** — wire server still shares one `Connection` for SQL execution;
  LISTEN/NOTIFY routing is per wire client PID.

### Catalog faithfulness

**Fixed:**

- **`pg_database` rows for ATTACH'd schemas** — `PgDatabaseCursor::load_databases()` includes
  main db + attached schema databases; `\l` shows them.
- **`pg_attribute.atttypmod`** — populated from varchar/numeric type modifiers.
- **`pg_authid` / `pg_user` / `pg_enum`** — role registry + enum labels from `CREATE TYPE ...
  AS ENUM` via `pg_enum` + stable enum type OIDs (60000+).
- **Enum OID assignment** — sequential from `USER_ENUM_OID_BASE` (60000), sorted by type name;
  no longer hash-mod-10000 colliding with `pg_attrdef` (50000+).
- **`pg_proc` stable OIDs** — sorted name map from base 80000; alias names (`char_length`,
  `btrim`, etc.) included.
- **Catalog index fast paths** — `pg_class` OID/relname equality; `pg_attribute` attrelid filter.
- **Multi-user roles** — in-memory `CREATE ROLE` / `DROP ROLE`; `pg_roles` / `pg_authid` /
  `pg_user` / `pg_get_userbyid` read live registry.
- **`pg_description`** — `COMMENT ON TABLE/COLUMN/TYPE` stored per connection; exposed via
  `pg_description` virtual table.
- **`pg_collation`** — built-in rows (`default`, `C`, `POSIX`, `ucs_basic`).
- **Bare `NUMERIC` catalog** — unqualified `NUMERIC` DDL maps to `numeric(38,19)`; `pg_type` /
  `pg_attribute` report OID 1700 / `numeric`.

**Remaining:**

- **Role persistence** — registry resets on reconnect; no password verification / `GRANT`.

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
- **`SET sql_dialect` via PG `SET`** — InternalHelper PRAGMA translation no longer restores the
  pre-SET dialect, so `SET sql_dialect = 'sqlite'` after Postgres mode persists correctly.
- **SQL-level PREPARE / EXECUTE / DEALLOCATE** — session registry in `pg_prepared`; `EXECUTE`
  binds parameters and returns a runnable statement.

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

**Fixed:**

- **Date validation** — rejects invalid calendar dates (e.g. Feb 31).
- **DDL string-replace** — word-boundary aware `convert_sqlite_ddl_to_postgres`.
- **JSON validation** — grammar check beyond bracket balance.

### Dead code

- `parser_pg/src/{ast,lexer,parser,token}.rs` (~5k LOC hand-written PG parser) **unused** —
  execution uses libpg_query. AGENTS.md confirms.
- `information_schema.tables → sqlite_master` map (`translator.rs:81`) is **dead** — schema
  prefix is stripped before it's reached.

## Next wave (queued)

Work in priority order; each item = branch off `pgmicro-fixes` → PR → squash merge.

| # | Branch | Scope | Notes |
|---|--------|-------|-------|
| 1 | `fix/auth-persist` | `core/pg_role.rs` + storage | Persist roles; wire startup user; `SET ROLE` |
**Completed on `pgmicro-fixes`:** interval/money types; wire binary params; `pg_database` schemas;
`atttypmod`; catalog validation; `pg_authid`/`pg_user`/`pg_enum`; dialect `SET sql_dialect`
persistence; NUMERIC wire precision; row streaming; stable `pg_proc` OIDs; catalog OID fast
paths; multi-user role registry; `pg_description` + COMMENT ON; COPY wire STDIN/STDOUT (text +
binary); COPY wire tests; optional TLS; query cancellation; LISTEN/NOTIFY pub/sub; `pg_collation`;
bare NUMERIC catalog metadata; SQL-level PREPARE/EXECUTE/DEALLOCATE; `pg_proc` PG alias names.

## Bottom line

Core engine works and the design is clean. The major silent-correctness cluster from the
original eval is fixed. Tooling fidelity (catalog, wire COPY, comments, roles, streaming,
NUMERIC) is substantially improved — psql `\du`, `\d+`, `\l`, and COPY workflows work for
common cases.

Remaining gaps cluster in two buckets:

1. **Production auth** — persist roles, password verification, `GRANT`/`REVOKE`.
2. **Wire session isolation** — per-client SQL connections instead of one shared `Connection`.

Strongest part: translator breadth + test depth + rapid correctness fixes on `pgmicro-fixes`.
Weakest: durable multi-session auth for anything beyond localhost dev.