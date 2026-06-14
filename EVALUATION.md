# pgmicro — Code Evaluation

In-process PostgreSQL front-end on Turso's SQLite-compatible engine. PG SQL → libpg_query
(C FFI, PostgreSQL's real parser) → protobuf → translator → Turso AST → shared SQLite
compile/exec pipeline. Plus PG catalog virtual tables, system functions, a wire-protocol
server, and a REPL with psql meta-commands.

## Does it work?

Yes, for core OLTP. ~400 tests, exercised end-to-end (binary stdin/stdout, Connection API,
wire TCP). Real coverage of SELECT/INSERT/UPDATE/DELETE, DDL, CTEs, UNION, window functions,
subqueries, arrays, schemas, catalog tables, COPY FROM. Not a demo — the actual execution
path is sound. The architecture (translate AST directly, never re-serialize SQL) is the
right call.

## What it actually does

| Layer | Real capability |
|---|---|
| Translator | Full DML, most DDL, ~all common expressions/operators, JSON `->`/`->>`, ILIKE, SIMILAR TO, BETWEEN, IS DISTINCT, CASE, COALESCE, casts, ARRAY, window frames, RETURNING, ON CONFLICT |
| Catalog | `pg_class/attribute/namespace/type/index/constraint/attrdef/proc/tables` populated from live schema; ~12 stubs for psql compat |
| Wire | Simple + Extended query protocol, multi-statement, trust auth (localhost-only; no TLS) |
| REPL | 19 `\` meta-commands, all tested |
| Schemas | CREATE/DROP SCHEMA → ATTACH separate `.db` files |

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
  `not: true`; `NOT` over `IN` subquery/list is folded correctly. Keyword `NOT IN` often
  worked via `NOT (IN …)`; the main gap was operator form and explicit `not` on `InSelect`.
- **NULLS FIRST/LAST in ORDER BY** — `translate_sort_nulls()` maps PG defaults (ASC → NULLS
  LAST, DESC → NULLS FIRST) and explicit `NULLS FIRST`/`LAST` to Turso `NullsOrder`.
- **DISTINCT ON** — rewritten via `row_number() OVER (PARTITION BY … ORDER BY …)` subquery
  (`wrap_distinct_on()`). Requires `ORDER BY`; not supported on VALUES or compound SELECT.

**Remaining:**

- **INTERSECT/EXCEPT ALL lose ALL**, `translator.rs:1405`. Dedups where PG would not.
- **GREATEST/LEAST → scalar MAX/MIN**, `translator.rs:2002`. SQLite scalar max/min take 2
  args; 3+ args invoke the aggregate → wrong/error. NULL semantics differ too.
- **Multi-table TRUNCATE / multi-cmd ALTER**: only first item processed silently
  (`translator.rs:857`, `:503`).
- **Aggregate ORDER BY dropped** — `array_agg(x ORDER BY y)` loses ordering,
  `translator.rs:2761`.
- **`= ANY(array)` stubbed to `0`** (always false), `translator.rs:2228`. Hack for catalog
  stubs, misfires on real queries.
- **`IS TRUE` → `IS 1`**: value `2` (truthy in PG) fails the test, `translator.rs:1955`.
- **MONEY→REAL, INTERVAL→TEXT**: interval arithmetic breaks; money rounds.

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

**Partially fixed:**

- **`SET`/`SHOW`/`RESET`/`RESET ALL` for `search_path`** — stored on `Connection.pg_search_path`;
  `SHOW search_path` and `RESET` (including `RESET ALL`) work. Other `RESET name` is a no-op.
  **`SET LOCAL search_path`** is accepted but session-scoped (not rolled back with the txn).
- **Unqualified name resolution** still hardcodes `public` — `search_path` is not wired into
  table/column lookup yet.

**Remaining:**

- **`SET LOCAL` transaction scope** — values persist for the connection lifetime; psycopg2
  `SET LOCAL` inside a txn does not restore on rollback.
- **Cross-schema txns not atomic** — separate ATTACH WAL files, partial commit possible,
  undocumented.
- **`:memory:` + CREATE SCHEMA** writes a physical file to cwd; sessions collide.
- **`parse_postgresql_sql` translates only `nodes()[0]`** — direct multi-statement `prepare()`
  silently drops the rest.

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

## Bottom line

Core engine works and the design is clean — direct-to-AST translation on a real DB is
genuinely useful, not faked. The gaps cluster in three buckets:

1. **Silent wrong-answer translations** (set-op ALL, GREATEST/LEAST, `= ANY(array)`, multi-table
   TRUNCATE) — violate the project's own "reject loudly over wrong results" principle. Highest
   priority now that NOT IN, NULLS ordering, DISTINCT ON, and schema path traversal are fixed.
2. **PG fidelity for tooling** (SQLSTATE codes, binary format, role/catalog stubs,
   search_path resolution) — ORMs/typed drivers misbehave; psql mostly survives.
3. **Wire server hardening** — auth/TLS if ever exposed beyond localhost; prepared-statement
   schema file cleanup.

Strongest part: translator breadth + test depth. Weakest: silent-correctness shortcuts in the
translator and wire-protocol tooling fidelity.
