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
| Translator | Full DML, most DDL, ~all common expressions/operators, JSON `->`/`->>`, ILIKE, SIMILAR TO, BETWEEN, IS DISTINCT, CASE, COALESCE, casts, ARRAY, window frames, RETURNING, ON CONFLICT, GREATEST/LEAST, DISTINCT ON |
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

**Accepted for localhost-only use:**

- **No auth / no TLS** — `NoopHandler` still returns `AuthenticationOk` to any client that
  can reach the port. CLI help and startup stderr warn to bind `127.0.0.1` only. Intended for
  trusted local development, not public network exposure.

**Remaining:**

- **Prepared `DROP SCHEMA $1`** — extended-protocol cleanup still uses string matching on the
  raw query; parameterized drops do not trigger `.db` file deletion.

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

**Fixed (continued):**

- **Aggregate ORDER BY** — `FuncCall.agg_order` is translated into `FunctionCall.order_by`;
  planner stores `order_by` on `Aggregate` and emits a sorter path (ungrouped: dedicated
  `AggOrderMetadata` sorter; grouped: extra sort keys on the GROUP BY sorter) so `AggStep`
  runs in sorted order. `array_agg(x ORDER BY y)` works end-to-end.

**Remaining:**

- **MONEY→REAL, INTERVAL→TEXT** — interval arithmetic breaks; money rounds.

### Wire protocol fidelity

- **All errors = SQLSTATE `XX000`**, `cli/pg_server.rs:672`. ORMs branching on sqlstate
  (constraint, syntax, serialization-retry `40001`) all broken.
- **Binary param format unsupported**, `cli/pg_server.rs:441`. Binary bytes decoded as UTF-8
  → garbage/error. JDBC/psycopg3 binary mode breaks.
- **NUMERIC = f64** everywhere. Precision silently lost.
- **No row streaming**, `max_rows` ignored. Full result set buffered in memory.
- **No SSL, no cancellation, no COPY-over-wire, no LISTEN/NOTIFY.**
- **Type OID vs value mismatch**: INT4 column emits i64-encoded value; bare `NUMERIC`
  advertised as FLOAT8 but `NUMERIC(10,2)` as NUMERIC — inconsistent.

### Catalog faithfulness

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

**Remaining:**

- **`SET LOCAL` for other GUCs** — only `search_path` has txn-local restore; other `SET LOCAL`
  still leaks to session scope.
- **Cross-schema txns not atomic** — separate ATTACH WAL files, partial commit possible,
  undocumented.
- **`:memory:` + CREATE SCHEMA** writes a physical file to cwd; sessions collide.
- **Multi-statement `prepare()`** — `translate_stmts()` handles multi-command ALTER; most
  other statement types still use `nodes()[0]` only.

### Validation bugs

- Date validation accepts Feb 31 / Apr 31, `core/pg_catalog.rs:2958`.
- `gcd`/`lcm` overflow returns TEXT `"ERROR: ..."` as a row value instead of raising,
  `core/functions/postgres.rs:156`.
- `convert_to_postgres_ddl` naive string replace can rename columns containing `INTEGER`,
  `core/pg_catalog.rs:3427`.
- JSON validation checks bracket balance only, not grammar — `{"a":}` passes.

### Dead code

- `parser_pg/src/{ast,lexer,parser,token}.rs` (~5k LOC hand-written PG parser) **unused** —
  execution uses libpg_query. CLAUDE.md confirms.
- `information_schema.tables → sqlite_master` map (`translator.rs:81`) is **dead** — schema
  prefix is stripped before it's reached.

## Next wave (queued)

Work in priority order; each item = branch off `pgmicro-fixes` → PR → squash merge.

| # | Branch | Scope | Notes |
|---|--------|-------|-------|
| 1 | `fix/sqlstate-codes` | `cli/pg_server.rs` | Map `LimboError` variants to PG SQLSTATE (syntax, undefined_table, constraint, etc.) |
| 2 | `fix/gcd-lcm-error` | `core/functions/postgres.rs` | Raise on overflow instead of returning TEXT error row |
| 3 | `fix/prepared-drop-schema` | `cli/pg_server.rs` | Extended-protocol `DROP SCHEMA $1` triggers `.db` cleanup |
| 4 | `fix/interval-money-types` | `parser_pg` + types | INTERVAL/MONEY type fidelity (larger; may need Turso-core types first) |

## Bottom line

Core engine works and the design is clean. The major silent-correctness cluster from the
original eval (NOT IN, NULLS, DISTINCT ON, set-op ALL, GREATEST/LEAST, ANY(array), TRUNCATE,
ALTER, IS TRUE, search_path) is now fixed or partially fixed.

Remaining gaps cluster in three buckets:

1. **Execution gaps** (INTERVAL/MONEY types) — translation is often done; runtime/compiler
   support lags.
2. **PG fidelity for tooling** (SQLSTATE codes, binary format, role/catalog stubs,
   `pg_database` attached schemas) — ORMs/typed drivers misbehave; psql mostly survives.
3. **Wire server hardening** — auth/TLS if ever exposed beyond localhost; prepared-statement
   schema file cleanup.

Strongest part: translator breadth + test depth + rapid correctness fixes on `pgmicro-fixes`.
Weakest: wire-protocol tooling fidelity and catalog completeness for ORM/framework startup.