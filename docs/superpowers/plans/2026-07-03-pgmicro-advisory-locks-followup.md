# pgmicro Plan: Wire `pg_advisory_lock`-family functions to the core registry

**Depends on:** `2026-07-03-turso-core-advisory-locks.md` landing first (adds
`AdvisoryLockRegistry`, the session-scoped and transaction-scoped counted
lock tables, and the `Connection`-level hooks). Do not start the SQL-facing
work in this plan before that lands — there is no registry to call into.

## Blocked on Task A3 (separate from the dependency above)

This is a second, independent blocker — do not conflate it with the core-plan
dependency above; the core plan can land and this plan still cannot ship.

Confirmed by direct read, not assumed: `cli/pg_server.rs` currently serves
**every** connected wire client through one shared `Connection` behind a
`Mutex`. Every query-handling call site fetches it the same way —
`let conn = self.conn.lock().unwrap().clone();` — at lines 430, 502, 551, 574,
and 879 (verify current line numbers before use, they shift as the file
changes), plus `self.conn.lock().unwrap()...` at line 103 and `self.conn.clone()`
at lines 107 and 239. This is exactly the master plan's **Task A3** ("Open one
`Connection` per accepted socket", `docs/superpowers/plans/2026-07-02-pgmicro-fixes.md`,
search "Task A3 (C4.3)") — confirmed **not yet landed**: the shared-mutex
pattern above is still the current code, not a stale description.

The core plan's registry keys per-connection state on `Connection` identity.
While every wire client shares one `Connection`, every `psql` session is,
from the registry's point of view, indistinguishable from every other
`psql` session:

- Session A calls `pg_advisory_lock(42)` → acquires the lock on the shared
  `Connection`.
- Session B (a completely different `psql` process) calls
  `pg_advisory_lock(42)` on the same shared `Connection` → the registry sees
  the *same* connection re-acquiring a key it already holds, which the core
  plan's reentrancy rule (acquire twice → succeeds immediately, never
  blocks) says must succeed instantly. Session B silently gets a lock it
  should have blocked on.

That is a correctness regression in the exact opposite direction of the
feature's purpose — the feature would create a false sense of mutual
exclusion between real, separate PostgreSQL clients. **Do not wire any
`pg_advisory_lock`-family SQL function until Task A3 lands.** If there is
schedule pressure to ship something sooner, the only acceptable interim is a
translator/function-level fail-loud rejection ("advisory locks require
per-connection wire isolation, not yet available") — never a silent,
spuriously-shared implementation. This mirrors the project's existing
pattern of Task B17 (`DELETE ... USING`, rejected with a clear error until
its own two-plan dependency lands) and Task B2 (`GENERATED ... AS IDENTITY`,
same treatment).

## What this plan actually does (once both blockers clear)

### 1. Register the functions using the existing connection-scoped-function pattern

This is not new engineering — pgmicro already has three precedents for a PG
scalar function that needs `&Connection` access:
`exec_pg_get_user_by_id`, `exec_pg_get_constraintdef`, and
`exec_pg_get_indexdef` (`core/functions/postgres.rs`). Each is registered as
a `ScalarFunc` variant in `core/function.rs` (name string mapping around
lines 1490-1510, arity declared in the `&[1]` / `&[1, 2]` / `&[1, 3]` table
around lines 1040-1051 — `PgGetIndexDef => &[1, 3]` is the existing precedent
for "PG has this exact set of overload arities, nothing in between," which
this plan's two-arg vs. one-arg advisory-lock forms should copy), and
dispatched in `core/vdbe/execute.rs` by passing `&program.connection`
directly (see the `PgGetUserById`/`PgGetConstraintDef`/`PgGetIndexDef` match
arms, around lines 7150-7185, each calling
`exec_pg_get_*(&program.connection, ...)`). Follow this exact shape:

- `core/functions/postgres.rs`: add `exec_pg_advisory_lock`,
  `exec_pg_advisory_lock_shared`, `exec_pg_try_advisory_lock`,
  `exec_pg_try_advisory_lock_shared`, `exec_pg_advisory_unlock`,
  `exec_pg_advisory_unlock_shared`, `exec_pg_advisory_unlock_all`, and the
  `_xact_` variants (`exec_pg_advisory_xact_lock`, etc. — these skip the
  unlock functions entirely, since real PostgreSQL has no
  `pg_advisory_xact_unlock`: transaction-scoped locks only ever release
  automatically at commit/rollback, verify this against the PG function list
  before assuming otherwise). Each takes `&Connection` plus either one
  `i64` key or two `i32`s (`(hi, lo)`, packed via the core plan's
  `pack_key`).
- `core/function.rs`: new `ScalarFunc` variants, arity table entries
  (single-bigint form is `&[1]`; two-int form needs a **second** function
  name-to-variant mapping since PG treats
  `pg_advisory_lock(bigint)` and `pg_advisory_lock(int, int)` as two
  overloads of the same SQL name with different argument *types*, not just
  counts — confirm whether the existing arity-table mechanism
  disambiguates on argument type or only on count, since `&[1, 2]` alone
  doesn't tell the dispatcher which single-arg type maps to which
  registry call if a 2-arg all-int overload also exists at arity 2 for a
  different purpose; this needs checking against how the existing `Lpad`/`Rpad`
  `&[2, 3]` case resolves argument types, not just counts, before assuming
  the two forms can share one arity-table entry cleanly).
- `core/vdbe/execute.rs`: new match arms passing `&program.connection` and
  the registry accessor methods added by the core plan.

Shared-vs-exclusive (`_shared` suffix) functions only make sense once the
core plan's optional shared/exclusive mode ships — the core plan explicitly
left this **out of scope** as "no consumer yet." If this plan lands first
against a core registry that only has the plain counted mode, either wait
for the core plan to add shared/exclusive mode too, or land the plain
(non-`_shared`) functions first and flag the `_shared` variants as a
follow-up once the core registry grows that mode. Do not fake
shared/exclusive semantics on top of the plain registry — that would be
exactly the kind of translator-side polyfill this project's principle 1
("build on Turso, don't hack around it") rules out.

### 2. Session-scoped release on connection close

Verify this is automatic once Task A3 lands: the core plan wires session-scope
release into `Connection::close()` (`core/connection.rs:1752`), the same
call site as the existing `crate::pg_listen::unregister_connection(self)`
call. Once Task A3 gives each wire client its own real `Connection`, Task
A5 ("Close the per-connection `Connection` and unregister it on disconnect",
`docs/superpowers/plans/2026-07-02-pgmicro-fixes.md`, search "Task A5
(C4.5)") is the call path that invokes `Connection::close()` on client
disconnect — confirm Task A5's cleanup path actually reaches
`Connection::close()` (not just a cancel-registry unregister) before
assuming session-scoped advisory locks release correctly when a `psql`
session drops without calling `pg_advisory_unlock_all()` itself.

### 3. Transaction-scoped release on COMMIT/ROLLBACK

The core plan states `set_tx_state` (`core/connection.rs:3108`) is the single
choke point for every transaction-end path. Confirm the wire server's
COMMIT/ROLLBACK handling (both explicit SQL and any implicit
transaction-end path used by the extended query protocol) actually routes
through `set_tx_state` and not some other commit path that bypasses it —
this should be true by construction (there is one `Connection::commit`/
`rollback` implementation), but confirm rather than assume, since this is
exactly the kind of "close to correct but not independently checked" claim
this project's plans are meant to stop making on faith.

## Testing

Per this project's Core Principle 6 (REPL first, psql second) — but this
feature is one of the rare cases where the REPL alone cannot verify the
core property. The in-process REPL (`cargo run -p pgmicro -- :memory:`) is
a single connection by construction, so it can verify reentrancy (acquiring
the same key twice from the one connection succeeds and is still held after
one release) and the basic function surface (arity, return shape, error on
mismatched unlock), but it **cannot** exercise cross-session contention,
since there is only ever one session. Verifying that a second, truly
independent connection blocks (or gets `false` from a try-variant) requires
two real concurrent `psql` processes against the wire server:

```sql
-- session 1 (psql #1)
SELECT pg_advisory_lock(42);          -- returns void, acquires

-- session 2 (psql #2), while session 1 still holds it
SELECT pg_try_advisory_lock(42);      -- must return false
-- then, in session 1:
SELECT pg_advisory_unlock(42);        -- releases
-- back in session 2:
SELECT pg_try_advisory_lock(42);      -- must now return true
```

Also test, with two real `psql` sessions:
- Session-scoped lock held across multiple statements within one session,
  released only by explicit unlock or the session disconnecting (kill the
  `psql` process, then have a second session try-acquire the same key and
  confirm it now succeeds).
- Transaction-scoped lock (`pg_advisory_xact_lock`) auto-releasing on both
  `COMMIT` and `ROLLBACK` — test both, not just one, matching the core
  plan's own test list.
- The two-int-argument form (`pg_advisory_lock(int, int)`) and the
  single-bigint form colliding correctly when they pack to the same `u64`
  (per the core plan's `pack_key` contract) — pick a `(hi, lo)` pair and a
  bigint value that are meant to collide, verify one session's two-int
  acquire blocks the other session's single-bigint acquire of the packed
  equivalent value.

Add these as new tests in `pgmicro/tests/pgmicro.rs` (spawns the wire server
and drives real client connections — the existing pattern for
`wire_begin_on_one_client_does_not_affect_another`-style tests already
proves out multi-connection wire testing in this codebase, reuse that
harness rather than inventing a new one).

Also run: `cargo test -p turso_core advisory_lock` (core plan's unit tests,
regression only), `cargo fmt`,
`cargo clippy --workspace --all-features --all-targets -- --deny=warnings`.

## Out of scope

- Everything the core plan excludes (shared/exclusive mode unless it lands
  first — see note above; cross-process locking; deadlock detection).
- `pg_locks` catalog visibility — real PostgreSQL exposes held advisory
  locks (and all other lock types) via the `pg_locks` system view; grepping
  this codebase (`core/pg_catalog.rs`) turns up no `pg_locks` implementation
  at all today, for any lock type. Neither this plan nor the core plan adds
  it. Flagging this explicitly as a known gap rather than leaving it
  silently unaddressed: a `psql` user who runs `SELECT * FROM pg_locks`
  expecting to see their own advisory lock will get an empty/missing-relation
  result instead, with no signal that this is a real gap versus "no locks
  currently held." A future task should either implement `pg_locks` (likely
  its own separate stub-to-populated catalog task, following the pattern in
  CLAUDE.md's "Adding a new PG catalog table" workflow) or, at minimum,
  document the gap in user-facing docs.
- Fixing Task A3 itself — that is Workstream A's existing, already-scheduled
  task; this plan only depends on it, it does not re-scope or re-implement it.

## Migration / compatibility

Purely additive: new `ScalarFunc` variants, new dispatch arms, no change to
any existing function's behavior. The interim fail-loud rejection (if shipped
before Task A3 lands) is itself additive — it turns "silently wrong" (no
advisory-lock functions exist today, so calling one is currently just a
"function does not exist" error) into "explicitly documented as blocked,"
which is a strict improvement in error clarity, not a behavior change for
any working query.
