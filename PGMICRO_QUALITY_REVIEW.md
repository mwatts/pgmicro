# pgmicro Code Quality Review

**Date:** 2026-07-02
**Scope:** Full quality review of the PostgreSQL-on-Turso port — translator, PG catalog, wire protocol server, dialect/dispatch layer, REPL/packaging, and cross-cutting PG semantics.
**Method:** Six parallel read-only review agents, one per subsystem. No code changes. Findings marked *[repro]* were reproduced against a live `pgmicro` build; the rest are from static reading with file:line evidence.
**Nature of this document:** Issue report only. It recommends nothing be merged or changed here — it records defects for triage.

---

## Executive summary

pgmicro is an ambitious and, in several areas, genuinely well-built port. The architectural core is sound: it uses PostgreSQL's real C parser (libpg_query) so identifier case-folding and syntax acceptance come "for free"; NUMERIC is true arbitrary-precision `BigDecimal`, not lossy f64; custom Turso types enforce SMALLINT range, VARCHAR(n) length, DATE/TIME validity, and BOOLEAN strictness the right way; SQLSTATE error mapping on the wire is reasonably broad; schema-name-to-path conversion is allowlisted against traversal; and `runtests.sh` is rigorous. The internal `EVALUATION.md` is unusually candid about known gaps.

That said, the review surfaced **four categories of serious problems**:

1. **Remotely-triggerable whole-process crashes (DoS).** Because release/fuzz profiles set `panic = "abort"`, several reachable panics and unbounded allocations kill the *entire server* (all connections), not just one query. Any client can crash the backend with a single statement.
2. **Silent wrong results.** The project's stated principle is "reject unsupported syntax with a clear error rather than silently produce wrong results." Many *recognized-but-partially-handled* constructs violate this: `> ANY (subquery)` becomes `IN`, column-level CHECK / FOREIGN KEY / GENERATED constraints are silently dropped, plain `LIKE` is case-insensitive, integer division by zero returns NULL, `FOR UPDATE` and isolation levels are silently discarded, and DROP SCHEMA can resurrect old data.
3. **Not safe for concurrent or transaction-using clients.** The wire server shares a single `Connection` (and a single COPY buffer and cancellation target) across every accepted socket, and never wires up wire-level transaction status. Real connection pools, ORMs, or two concurrent `psql` sessions will see cross-talk and wrong transaction semantics.
4. **Catalog fidelity gaps that break real introspection.** OIDs aren't stable across DDL, namespaces are hardcoded to `public`, multiple indexes can be marked primary key, and overloaded functions collide on OID — exactly the things psql `\d`, JDBC/npgsql/psycopg caches, and ORMs depend on.

Severity counts across all subsystems: **~8 Critical, ~20 High, ~30 Medium, plus numerous Low/code-quality items.**

---

## CRITICAL findings

### C1. Remote DoS: `gcd()`/`lcm()` integer-overflow panic aborts the process *[repro]*
`core/functions/postgres.rs:145-173`. The overflow guard only checks `(MIN,0)`, `(0,MIN)`, `(MIN,MIN)`; the Euclidean loop still panics on `i64::MIN % -1`. Reproduced: `SELECT gcd(-9223372036854775808, -1)` → `panicked ... remainder with overflow`. With `panic = "abort"` this takes down the whole server. The existing `gcd_overflow_raises` test only covers the three already-guarded tuples, giving false confidence.

### C2. Remote DoS: unbounded allocation in `repeat()`/`lpad()`/`rpad()` *[repro]*
`core/functions/postgres.rs:176-186` (`repeat`), `:121-143` (`lpad`), `:334-356` (`rpad`). No cap on the user-controlled length before allocation. Reproduced: `SELECT repeat('a', 9223372036854775807)` → `memory allocation failed` → `Abort trap: 6`. This is `handle_alloc_error` → `abort()`, uncatchable even by `catch_unwind`. Real PostgreSQL caps these and returns an ERROR.

### C3. Remote DoS: stack overflow in the hand-rolled JSON validator
`core/pg_catalog.rs:4143-4296` (`parse_json_value`/`parse_json_object`/`parse_json_array`), reachable via `pg_input_is_valid`/`pg_input_error_info`. No depth limit — a few tens of KB of nested `[` overflows the stack and aborts the process. Notably `core/json/jsonb.rs:16` already defines and enforces `MAX_JSON_DEPTH = 1000`; this is a duplicate reimplementation that lacks the guard.

### C4. Wire server shares one `Connection` across all clients — no session isolation
`cli/pg_server.rs:54,263` (`conn: Arc<Mutex<Arc<Connection>>>`), built once in `run_async` and handed to every socket. The mutex only guards cloning the `Arc`, not execution, so concurrent clients run on the *same* `Connection`, which holds per-session state (`auto_commit`, `transaction_state`, `attached_databases`, dialect, `search_path`, etc.). Client A's `BEGIN` wraps client B's `INSERT`; any client's `SET`/`PRAGMA` mutates every other client's behavior. `db.connect()` (`core/lib.rs:1763`) exists to hand out independent connections and is never used per-socket.

### C5. Query cancellation targets the shared connection, not the requester's query
`cli/pg_server.rs:205-217`. `on_cancel_request` calls `interrupt()` on the single shared `Connection`, so a CancelRequest from client A (or a stray one) aborts client B's in-flight query. Direct consequence of C4.

### C6. Concurrent `COPY FROM STDIN` sessions clobber each other's buffer/target
`cli/pg_server.rs:266` (`copy_in: Arc<Mutex<CopyInSession>>` on the single shared handler), `:833` (`on_copy_data` ignores `_client`), `:441,507` (`do_query` resets the shared buffer on a new COPY). If client B starts a COPY while A is mid-stream, A's buffered bytes are discarded and subsequent CopyData is applied to whichever table `session.stmt` last pointed at → data loss / data written to the wrong table.

### C7. `x <op> ANY (subquery)` silently mistranslated to `x IN (subquery)` for every operator except `=`
`parser_pg/src/translator.rs:3018-3020` and `:3659-3662` (`AnySublink` arm never inspects `oper_name`; the sibling `AllSublink` arm does check). `SELECT * FROM t WHERE price > ANY (SELECT price FROM other)` becomes `price IN (...)` — a completely different result set, no error. Same for `<`, `<=`, `>=`, `<>`.

### C8. DROP SCHEMA resurrects stale data on re-CREATE *[repro]*
`core/pg_dispatch.rs:228-246` (`handle_pg_drop_schema`). For a non-`public` attached schema without `CASCADE`, tables are not dropped and there is no empty-check (unlike the public-schema path at `:249-271`); it goes straight to `detach_database`, which never unlinks the backing file. Reproduced: `CREATE SCHEMA s; CREATE TABLE s.t(x INT); INSERT INTO s.t VALUES(42); DROP SCHEMA s; CREATE SCHEMA s; SELECT * FROM s.t;` → returns `42`. An app doing DROP+CREATE to reset a tenant/test schema gets the old data back. (File-cleanup exists only in the wire frontend `cli/pg_server.rs`, so the REPL, NAPI, and embedders leak `.db` files on every DROP SCHEMA — see H-series.)

---

## HIGH findings

### Translator (`parser_pg/src/translator.rs`)
- **H1. Column-level `CHECK` constraints silently dropped in CREATE TABLE.** `:336-349` (catch-all `_ => {}`). Table-level CHECK is handled; inline `balance numeric CHECK (balance >= 0)` is discarded → negative inserts succeed.
- **H2. `GENERATED`/`IDENTITY` columns silently dropped** in both CREATE TABLE (`:336-349`) and ALTER TABLE (`:706-729`). Produces plain nullable columns, no error.
- **H3. `CAST(x AS <unmapped type>)` silently drops the cast**, bypassing custom-type validation. `:2328-2338`; `pg_type_name_to_ast_type` (`:4714-4747`) returns `None` for every user ENUM/domain → `'bogus'::mood` succeeds instead of raising `invalid input value for enum`.
- **H4. `DISTINCT ON` degrades to plain `DISTINCT` inside UNION/INTERSECT/EXCEPT branches.** `:1863-1866`.
- **H5. `@>` / `<@` / `&&` unconditionally mapped to array functions**, mistranslating the extremely common JSONB containment idiom `metadata @> '{"k":"v"}'`. `:2936-2953`. (Also flagged independently by the semantics agent.)
- **H6. `~*` / `!~*` (case-insensitive regex) mapped to case-sensitive REGEXP.** `:2836-2855`; `core/regexp.rs:22` compiles without `(?i)`. `'ABC' ~* 'abc'` returns false.
- **H7. Multi-object `DROP a, b, c` acts only on the first; `CASCADE`/`RESTRICT` ignored.** `:816-820`.
- **H8. `DELETE ... USING` clause ignored.** `:1253-1281` (contrast working `UPDATE ... FROM`).
- **H9. ALTER TABLE ADD COLUMN silently drops FK/CHECK/GENERATED/IDENTITY.** `:675-743` — divergent, duplicated logic vs CREATE TABLE path (root cause shared with H1/H2).
- **H10. `ON CONFLICT ON CONSTRAINT name` drops the constraint target**, widening DO NOTHING/DO UPDATE to any conflict. `:3796-3891`.
- **H11. `LIKE ... ESCAPE`/`ILIKE ... ESCAPE` mistranslated** to a nonexistent `like_escape` function call rather than being unwrapped (the pattern used correctly for `SIMILAR TO ... ESCAPE`). `:3055-3182`.
- **H12. Expression `CAST` drops type params and array dimensions.** `:4714-4747`; `'abcdefgh'::varchar(3)` isn't truncated, `'{1,2,3}'::integer[]` becomes a scalar cast.
- **H13. `information_schema.tables`/`.columns` mapping is dead code.** `map_table_name` (`:75-108`) matches a dotted string, but callers only pass the bare `relname` — contradicts CLAUDE.md's documented capability.

### Cross-cutting semantics
- **H14. Plain `LIKE` is case-insensitive (PG is case-sensitive).** `core/vdbe/value.rs:1178-1213` uses `eq_ignore_ascii_case`; the translator sets no `case_sensitive_like` for the PG dialect. Affects **every** `LIKE` predicate — highest-reach silent-correctness bug found.
- **H15. `SELECT ... FOR UPDATE`/`FOR SHARE`/`NOWAIT`/`SKIP LOCKED` parse but are completely discarded.** `translator.rs` never reads `SelectStmt.locking_clause`. Apps relying on row locks get silent lost updates. `parse_valid.rs:test_for_update` only checks parseability, giving false confidence.
- **H16. Transaction isolation levels silently discarded.** `translator.rs:492-524`; `txn.options` (isolation/read-only/deferrable) never read. `BEGIN ISOLATION LEVEL SERIALIZABLE` behaves like bare `BEGIN`.
- **H17. Plain integer/float division by zero returns NULL instead of erroring.** `core/numeric/mod.rs:152-165`. Only NUMERIC/INTERVAL/MONEY division raises the PG-correct `22012`; the far more common `int4`/`int8`/`real` division silently yields NULL.

### Dialect / dispatch
- **H18. `SET <guc>` has no PG-GUC allowlist; common client GUCs hard-error.** *[repro]* `core/pg_dispatch.rs:70-81` maps any `SET name = value` → `PRAGMA name = value`; unknown names hard-fail. `SET client_encoding = 'UTF8'` and `SET application_name = 'psql'` both error `Not a valid pragma name`. Most real PG drivers (Npgsql, JDBC, PgBouncer) issue these on connect — the single most likely thing to break "connect with a real client."
- **H19. `EXECUTE $N` parameter substitution corrupts SQL on placeholder collisions.** *[repro]* `core/pg_dispatch.rs:421-437` uses `replacen("$1", val, 1)`; `"$1"` is a prefix of `"$10"`/`"$11"`, so ≥10-param prepared statements (or `$1` inside a string literal) silently bind wrong values.
- **H20. Multi-name `DROP SCHEMA a, b` silently drops only the first.** *[repro]* `translator.rs:5082-5113` uses `objects.first()`.

### Wire server
- **H21. BEGIN/COMMIT/ROLLBACK never update wire transaction status; `ReadyForQuery` byte is wrong.** `cli/pg_server.rs:962-968` always returns `Response::Execution`, never `TransactionStart`/`TransactionEnd`, so status stays `'I'` after BEGIN and never becomes `'E'` on failure. `BEGIN; <failing INSERT>; SELECT 1;` executes the SELECT instead of rejecting it ("current transaction is aborted"). Breaks psql prompt, `PQtransactionStatus`, poolers, ORMs. Untested.
- **H22. Binary result-format requests silently return text bytes.** `cli/pg_server.rs:1386-1450`. NUMERIC and array encoders hard-code `FieldFormat::Text`; the generic `Value::Text` branch calls `encode_field` which, for a Binary schema, emits raw UTF-8 via `<&str as ToSql>::to_sql` with no `accepts()` check. Any client requesting binary for NUMERIC/array/DATE/TIME/TIMESTAMP(TZ)/UUID/INTERVAL/MONEY/INET/JSON(B) gets a protocol violation. JSONB binary also omits the required version byte.
- **H23. Simple-query multi-statement batch drops already-computed responses on a later error.** `cli/pg_server.rs:420-478` uses `?` mid-loop; non-SELECT statements execute eagerly with real side effects, but on statement N's failure the client sees only one ErrorResponse — no CommandComplete tags for the already-applied writes 1..N-1. Risks duplicate retries.

### Catalog
- **H24. `pg_class.oid`/`pg_attribute.attrelid` are not stable — they shift on ordinary DDL.** `core/pg_catalog.rs:148-156,773-777`. OIDs are index-into-alphabetically-sorted-table-list; creating an alphabetically-earlier table reassigns every later relation's OID. Breaks any client caching relation OIDs.
- **H25. `pg_index.indisprimary` mislabels multiple indexes as PK.** `:2947-2951`. SQLite names every implicit unique index `sqlite_autoindex_*`, so PK-backing and plain UNIQUE-backing are indistinguishable → `CREATE TABLE t (a TEXT PRIMARY KEY, b TEXT UNIQUE)` yields two rows both `indisprimary=1`.
- **H26. Attached-schema tables invisible to `pg_class`/`pg_attribute`.** `:324-453,769-824` read only the main schema (`pg_namespace` does enumerate attached ones). `CREATE SCHEMA foo; CREATE TABLE foo.bar(...)` — `foo.bar` never appears in `pg_class`.
- **H27. `relnamespace`/`pronamespace` hardcoded to 2200 (public).** `:359,416,1498,1541,1580`. Cross-schema and system-object joins are meaningless; `WHERE pronamespace = 'pg_catalog'::regnamespace` returns no builtins.
- **H28. `pg_attribute.attlen=-1` with `attbyval=true` — an impossible combination** for every varlena column. `:798,803`.
- **H29. Hidden/generated columns leak into `pg_attribute` and inflate `relnatts`.** `Column::hidden()` never checked. `:784-820,348`.
- **H30. Built-in aggregates mislabeled `prokind='w'` (window) instead of `'a'`.** `:1488-1492,1532-1536`. `\da` and ORM aggregate enumeration return nothing.
- **H31. `pg_constraint.conindid` resolved via unscoped substring/prefix scan** → can point to a wrong/unrelated table's index (`foo` vs `foobar`; `users(email UNIQUE)` matching `idx_orders_email_lookup`). `:3206-3224`.
- **H32. `convert_sqlite_ddl_to_postgres` regex corrupts string literals in DEFAULT/CHECK.** `:4298-4346`. `DEFAULT 'REAL ESTATE'` → `DEFAULT 'double precision ESTATE'` in `pg_get_tabledef` output.

### REPL / packaging
- **H33. `run_stdin()` silently truncates scripts on invalid UTF-8 and exits 0.** *[repro]* `pgmicro/src/main.rs:989`; `read_line(...).unwrap_or(0) == 0` treats a decode error as EOF. `printf 'SELECT 1;\n\xff\xfe\nSELECT 2;\n' | pgmicro :memory:` runs `SELECT 1`, drops `SELECT 2`, exits 0. Violates "Fail Loud" on the primary non-interactive path.
- **H34. Case-insensitive identifier collision breaks quoted-identifier semantics.** *[repro]* `CREATE TABLE "Foo"(...); CREATE TABLE foo(...)` → `table foo already exists`. Storage rides on case-insensitive SQLite; `\d "Foo"` also never strips quotes. Untested end-to-end.

---

## MEDIUM findings

### Translator
- Boolean literals `TRUE`/`FALSE` become `Literal::Numeric("1"/"0")` instead of `Literal::True`/`False` (`:2605-2610`) — risks `pg_typeof(true)`/wire type-inference divergence.
- `ON CONFLICT` action catch-all drops the whole clause on unrecognized action (`~:3845`).
- Expression-based `ON CONFLICT (lower(email))` targets silently filtered out (`:3849-3868`).
- `CHAR(n)` mapped to the same "varchar" type as `VARCHAR(n)` — loses blank-padding (`:4497-4502`).
- `TIME`/`TIMETZ` collapse to one "time" type, losing offset (`:4488,4726`).
- Bare `NUMERIC` hardcoded to `numeric(38,19)` rather than arbitrary precision (`:4503-4509`).
- `def_elem_bool_val` treats unrecognized strings as `false` (`:4400-4407`) — `HEADER 'nope'` → HEADER false.
- COPY options (incl. `ENCODING`) silently ignored (`:4269-4290` + 3 duplicated extractors) — potential silent data corruption on import.
- `TRUNCATE ... RESTART IDENTITY`/`CASCADE` ignored; TRUNCATE → plain DELETE, sequences never reset (`~:892-921`).
- `CREATE OR REPLACE VIEW` (`replace` ignored → "already exists") and matview `WITH NO DATA` (`skip_data` ignored → populated immediately) (`:923-1031`).
- `CREATE INDEX` drops `USING <method>`, `INCLUDE (...)`, `NULLS NOT DISTINCT` (`:745-807`).
- SERIAL/autoincrement misattribution when a table has both a SERIAL column and a separate table-level PK (`:184-246,325-386`).
- `WITHIN GROUP` (ordered-set aggregates) never read — latent trap if such aggregates are added to Turso (`:3224-3338`).

### Semantics
- **TIMESTAMPTZ has no real timezone semantics** — `core/schema.rs:685` encodes identically to TIMESTAMP; the wire layer just appends `+00` (`cli/pg_server.rs:1421-1430`). No `AT TIME ZONE`, no `SET TIME ZONE`/`TimeZone` GUC, no input normalization.
- **INT4/INTEGER has no 32-bit range enforcement** — `translator.rs:4512` maps straight to SQLite's 8-byte INTEGER; SMALLINT *does* get a range CHECK, so this is an asymmetric omission (`core/schema.rs:686`).
- **No `CREATE SEQUENCE`/`nextval`/`currval`/`setval`** (`translator.rs:100-146` catch-all). `pg_dump` output for SERIAL/IDENTITY columns won't replay. Fails loudly at least.
- **No advisory locks** (`pg_advisory_lock` family) — absent; fails cleanly as unknown function.

### Dialect / wire
- `cli/pg_server.rs::drop_schema_name` (`:291-322`) is a second, naive `starts_with("drop schema")` re-parser deciding file cleanup — a leading comment defeats it, leaking `.db` files. File-lifecycle logic belongs in `handle_pg_drop_schema`.
- Schema-file cleanup exists only in the wire server; REPL/NAPI/embedders leak `turso-postgres-schema-*.db` on every DROP SCHEMA (same root cause as C8).
- Synchronous, unyielding query/COPY execution blocks the tokio worker thread (`cli/pg_server.rs:906-959,695-821`), delaying other clients' scheduling, cancellation, and NOTIFY flush.

### Catalog
- `stable_proc_oid_map` keys by name only, not name+arity — overloaded builtins (`round/1` vs `round/2`) share one OID (`:1456-1474`).
- Three catalogs each restart OID allocation at 16384 with no global counter (`:16,608,1755,1765`) — a landmine for any future generic OID resolver.
- `reltype` hardcoded to 0 for every relation (`:360,417`).
- Unchecked slice index in `parse_enum_labels_from_type_def` (`:896`) can panic on a malformed persisted `TypeDef.sql`.
- `filter()` clones/rescans the full schema each call → O(T²) for `pg_class JOIN pg_attribute` (`:325,329,775-782`); `pg_get_constraintdef`/`pg_get_indexdef` similarly O(N²) (`:4661-4788`).
- `pg_get_indexdef` arity `[1,2]` doesn't match real PG's `(oid)`/`(oid,col,pretty)`; 3-arg form rejected, fabricated 2-arg form ignores its second arg (`core/function.rs:1046`).
- `pg_trigger` stub missing `tgparentid` (PG13+) (`:3658`).
- User enum `typnamespace` hardcoded to `pg_catalog` (11) instead of its schema (`:2800`).
- `pg_type` `money` row has `typbyval:false`; real PG is `true` (`:2594`).
- Unnamed CHECK constraints collide on `conname` (`t_check` with no suffix) (`:3322-3326`).
- Table-OID logic duplicated in `pg_comment.rs:63-88` and already drifted (lowercases the key; `pg_catalog.rs` doesn't) → `pg_description.objoid` can desync from `pg_class.oid`.

### REPL / packaging
- `\q` bypasses `rl.save_history()` via direct `process::exit(0)` (`pgmicro/src/main.rs:891-892`) — the canonical quit path silently discards history.
- `COPY <table> FROM STDIN` in REPL/non-server mode fails with a confusing cascading error; no graceful "requires --server" message (`pgmicro/src/main.rs`).
- EVALUATION.md's "19 meta-commands, all tested" is false: `\q` and `\dg` are unexercised.
- `npm/pgmicro/index.js` version guard hardcodes `0.0.3` while `package.json` is `0.0.5` — stale generated loader (enforcement is opt-in, so latent).
- `npm/pgmicro/cli.js:9-14` maps `darwin-x64` → the arm64 package with no x64 target built — genuine Intel Macs exec an incompatible binary instead of getting a clean "unsupported platform" message.

---

## LOW / code-quality findings

- No unguarded `unwrap()`/`expect()`/`panic!` reachable from user SQL was found in the translator (~5,917 non-test LOC) or from malformed wire bytes — decoding paths return `Result`. The crash bugs (C1–C3) are arithmetic/allocation/recursion, not indexing.
- **Duplicated, divergent logic** is the structural root cause of several bugs: constraint handling forked between `translate_create_table_column` and `translate_column_def` (→ H1/H2/H9); COPY extraction forked across 4 functions with inconsistent unknown-FORMAT handling; ~300+ lines of near-identical vtab cursor boilerplate across 15+ impls in `pg_catalog.rs`; a hand-rolled JSON validator duplicating depth-guarded `core/json/` logic (→ C3); OID derivation reimplemented in `pg_comment.rs` (→ drift).
- Catalog `attalign`/`attstorage`/`attidentity`/`attgenerated` hardcoded; unnamed column → `attname=""` masks anomalies instead of failing loud.
- `pg_encoding_to_char()` maps every unknown encoding to `"UTF8"` instead of erroring.
- Quoted role identifiers lowercased (`CREATE ROLE "Alice"` → `rolname='alice'`).
- Cosmetic: `pg_get_tabledef` leaves a stray trailing space after `SERIAL PRIMARY KEY`.
- Dead files confirmed: `parser_pg/src/{ast,lexer,parser,token}.rs` (~5,370 LOC) are not declared as modules anywhere and referenced nowhere — genuinely dead, not just an unused path. `test_for_update` etc. in the dead parser's tests give false confidence about locking support.
- Two catalog tests assert nothing while appearing to pass: `test_postgres_pg_class` (assigns to `_` and asserts nothing) and `test_postgres_pg_attribute` (`COUNT(*)==0` with no table created; stale comment).

---

## Test coverage gaps (aggregated)

Biggest untested areas, several overlapping exactly with confirmed bugs:

- **Error/reject paths in the translator** — only 3 of 108 unit tests assert `Err`, against dozens of `Err` sites. No coverage for CREATE TABLE constraints, CREATE INDEX, DROP CASCADE/multi-object, regex operators, `DISTINCT ON`, JSON operators, general CAST, `= ANY` vs other operators — i.e. where C7/H1/H4/H6 live.
- **Concurrent wire clients** — zero tests with two racing clients, despite the shared-`Connection` architecture (C4–C6).
- **Wire transaction semantics** — no test inspects the `ReadyForQuery` status byte or BEGIN/COMMIT/ROLLBACK over the wire (H21); exactly one transaction-adjacent test exists overall.
- **Binary result format** — no test requests binary for NUMERIC/array/date-time/UUID (H22).
- **Multi-schema catalog** — no test creates a table in a non-public schema and checks `relnamespace`/`schemaname` (H26/H27); `pg_get_tabledef` has zero coverage.
- **Multi-table catalog joins** — `pg_class`+`pg_attribute`+`pg_namespace` (the real `\d`/ORM shape) is never joined; FK `confrelid`/`confkey` never asserted.
- **Quoted/mixed-case identifiers** — never passed to `\d`/`\dt` or catalog SQL (H34).
- **Malformed REPL input** — no invalid-UTF8/garbage-byte stdin test (H33).
- **DoS inputs** — `gcd`/`repeat` overflow tests exist but only cover already-guarded cases (C1/C2).
- **npm package** — 5 vitest cases only; no `cli.js` or platform-resolution test.

---

## Confirmed strengths (for balance)

- Uses the real libpg_query C parser → PG identifier case-folding (fold-unquoted-lowercase, preserve-quoted) is correct "for free."
- NUMERIC is true arbitrary-precision `BigDecimal` with proper div-by-zero errors (`core/schema.rs:693`, `core/vdbe/execute.rs:8046-8077`).
- SMALLINT range, VARCHAR(n) length, DATE/TIME calendar validity, BOOLEAN literal strictness enforced via Turso custom-type CHECK/RAISE — the "build on Turso, don't hack around it" principle working as intended.
- `NULLS FIRST/LAST` defaults match PG (`translator.rs:3901-3913`); `IS DISTINCT FROM`, `ILIKE`, `= ANY(array)` handled correctly.
- SQLSTATE mapping on the wire is broad (~25 explicit mappings) rather than one generic code (`cli/pg_server.rs:1705-1896`).
- Schema-name→path conversion is allowlisted and tested against traversal (`core/pg_schema.rs:15-27`); dialect save/restore around internal prepares is closure-scoped and error-path safe (`core/connection.rs:774-852`).
- Core footprint is disciplined: PG-only logic lives mostly in dedicated `core/pg_*.rs` modules (~7,100 LOC), with small dialect-gated touch points elsewhere — deliberately structured to avoid upstream merge conflicts.
- `runtests.sh` correctly propagates failures (`PIPESTATUS`, accumulated failures, accurate exit code).

---

## Recommended triage order (issue-report guidance only — no changes made here)

1. **The four remote-crash bugs (C1–C3) and the shared-connection trio (C4–C6)** — these make the server unsafe to expose to any untrusted or concurrent client. Highest priority.
2. **Silent-wrong-result translator/semantics bugs (C7, C8, H1–H3, H14–H17)** — these violate the project's core principle and corrupt data/results with no error.
3. **Client-compatibility blockers (H18 GUC allowlist, H21 txn status, H22 binary format)** — required before real drivers/ORMs/poolers work.
4. **Catalog fidelity (H24–H32)** — required before `\d`/ORM introspection is trustworthy.
5. **Structural dedup (constraint handling, COPY extractors, vtab boilerplate, JSON validator, OID derivation)** — fixes multiple bugs at their shared root and shrinks future merge surface.
6. **Test coverage** — add error-path, concurrency, transaction, binary-format, and multi-schema tests; several would have caught the bugs above.
