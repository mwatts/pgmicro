# Turso-Core Plan: Cooperative connection-scoped advisory lock registry

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
follow-up pgmicro-side plan would be `2026-07-03-pgmicro-advisory-locks-followup.md`
(not written by this plan — see **Explicitly out of scope**).

## Problem

Turso has no engine-managed mutual-exclusion primitive for application-level
coordination. Applications that want "only one connection does X at a time"
today have to build it themselves on top of a real table (a `CREATE TABLE
locks (name TEXT PRIMARY KEY)` + `INSERT`/`DELETE` dance), which forces a
write transaction, participates in WAL/durability the caller didn't actually
want (the lock isn't data, so persisting it and recovering it after a crash is
pure overhead), and needs a cleanup path for the "connection died without
releasing" case (an orphaned row, not an engine-tracked resource that goes
away when its owning connection does).

A lightweight, in-memory, connection-scoped named-lock registry is a generic
engine capability, independent of any particular SQL dialect: acquire/release
a lock identified by an integer key, scoped either to the connection's whole
lifetime or to its current transaction, with blocking and non-blocking
acquisition variants. This is the same shape of problem Turso already solved
for pub/sub coordination — `PgNotifyHub` / `PgListenRegistry`
(`core/pg_listen.rs`) is a database-scoped hub plus a per-connection registry,
with connection-close cleanup wired into `Connection::close()`
(`core/connection.rs:1752`). An advisory lock registry is the same pattern
applied to mutual exclusion instead of pub/sub.

## Design

### Two distinct lifetime scopes

Two independent registries per connection, not one:

- **Session-scoped:** held until explicitly released or the connection
  closes. Acquiring twice from the same connection must succeed (see
  reentrancy below); it is not released by commit or rollback.
- **Transaction-scoped:** held until the current transaction ends, by commit
  **or** rollback — both, not just commit. `core/connection.rs:3108`
  (`set_tx_state`) is the single choke point every transaction-end path
  (`commit`, `rollback`, `Connection::close()`'s implicit rollback at
  `core/connection.rs:1733`, and `Drop`'s implicit rollback) already funnels
  through to reach `TransactionState::None` — that is the one hook needed to
  auto-release every transaction-scoped lock this connection holds, without
  duplicating release logic across each transaction-end call site.
- These two scopes are **separate lock tables**, not one table with a scope
  flag: a lock acquired session-scoped and a lock acquired transaction-scoped
  under the identical key are different lock instances that do not block each
  other and release independently. **Unverified, flag for psql check:** real
  PostgreSQL's session- and transaction-level advisory locks on the same key
  from the same backend interact in a way this document has not independently
  confirmed byte-for-byte (PG's own docs describe them as counted
  independently per level, but the exact behavior when a session-level and a
  transaction-level acquisition of the same key coexist on one backend needs
  a live check before the follow-up plan relies on it) — build the two
  registries structurally independent (this is the safe, generalizable
  design regardless of the answer) but do not assert the interaction
  behavior is nailed down until that check happens.

### Reentrant, counted locks

Each registry entry is `(key) -> count`, not `(key) -> bool`. Acquiring a key
already held by the same connection/transaction increments the count and
succeeds immediately (never blocks on your own hold — this is what makes it
"cooperative": the engine tracks ownership per requester, not a bare
mutex). Releasing decrements; the lock is actually freed (and can be acquired
by another connection) only when the count reaches zero. Releasing a key not
currently held by the caller is a no-op-with-signal (return "was not held"
rather than panicking or silently succeeding) — callers need to be able to
detect a mismatched acquire/release pair.

### Key space

A single `u64` is the lock namespace — one flat integer keyspace, engine
side. Some callers will want a two-part namespace (e.g., "class of lock" +
"instance id") without the engine imposing that structure; support this as a
pure bit-packing convenience (`pack_key(hi: u32, lo: u32) -> u64`), not as a
second parallel keyspace — a caller that packs two `u32`s and a caller that
passes one `u64` directly must collide correctly if they pick the same
64-bit value, since underneath it is one registry keyed on one integer type.

### Blocking vs. non-blocking acquisition

Turso is not a thread-per-connection blocking engine — bytecode execution is
an async state machine that yields `IOResult`/`StepResult::IO` rather than
parking a real OS thread (see `docs/agent-guides/async-io-model.md` and the
existing `core/busy.rs` busy-handler, which already solves exactly this
problem for page-level lock contention: instead of `thread::sleep`, it
returns a yield-with-timeout that the caller re-drives on the next `step()`).
A blocking advisory-lock acquire must follow the same shape: non-blocking
try-acquire is a synchronous call that returns immediately (`true` = took the
lock or already held it, `false` = held by someone else); blocking acquire is
built on top as a retry loop through the same yield-and-re-drive mechanism as
`BusyHandlerState`, not a new blocking primitive — this keeps a blocked
advisory-lock wait interruptible via the connection's existing
`is_interrupted()`/`interrupt()` mechanism (`core/connection.rs:3090-3097`)
the same way a long-running query already is, instead of inventing a second,
inconsistent cancellation path.

Optional shared-vs-exclusive mode is a natural extension of the same counted
registry (an exclusive holder excludes all other holders; shared holders
exclude only exclusive requesters and count together) but adds real
complexity — see **Explicitly out of scope**.

### Session identity — and an empirically-confirmed pgmicro dependency

The registry keys per-connection state on `Connection` identity (the same
model `pg_notify_subscriber_id` uses today — an opaque id lazily assigned to
a `Connection`, `core/pg_listen.rs:213-221`). This has **no dependency on
anything in pgmicro's wire layer** for the core feature itself: every
`Database::connect()` call already returns a distinct `Connection` today, so
the core registry is fully buildable and unit-testable right now with two
plain `Connection`s and no wire protocol involved at all.

**Confirmed by reading `core/connection.rs` and
`docs/superpowers/plans/2026-07-02-pgmicro-fixes.md` (Task A2, line ~4214),
not assumed:** pgmicro's wire server (`cli/pg_server.rs`) currently holds and
serves **every** connected wire client through one single shared
`Connection` — Workstream A Task A3 ("open one Connection per accepted
socket") is not yet landed. This means the core feature itself has no
dependency, but **the pgmicro-side follow-up plan does**: wiring this
registry to PG advisory-lock SQL functions before Task A3 lands would make
every wire client observably the same "session" for locking purposes —
two unrelated `psql` connections would see each other's session-scoped
locks as already-held-by-me (spuriously reentrant) rather than
held-by-another-session (correctly blocking). The follow-up plan must
either wait for Task A3 or explicitly document this as a known-broken
interim state; it must not silently wire the SQL functions up early.

### Files touched

- New file `core/advisory_lock.rs`: `AdvisoryLockRegistry` (the counted
  `HashMap<u64, count>` pair — session-scoped and transaction-scoped — plus
  acquire/release/try-acquire methods), following the `PgNotifyHub` /
  `PgListenRegistry` split (database-scoped shared state vs. per-connection
  view) already established in `core/pg_listen.rs`.
- `core/connection.rs`: add the two registry fields to `Connection` (mirrors
  `pg_listen`/`pg_notify_subscriber_id` at lines 242-244); call the
  transaction-scoped release from `set_tx_state` (line 3108) on every
  transition into `TransactionState::None`; call the session-scoped release
  from `Connection::close()` (line 1752), same pattern as the existing
  `crate::pg_listen::unregister_connection(self)` call there.
- `core/busy.rs` or a thin wrapper alongside it: the yield-and-retry adapter
  used by blocking acquire, reusing `BusyHandlerState`'s delay/iteration
  logic rather than duplicating it.

## Explicitly out of scope

- **Any PostgreSQL-facing SQL function** (`pg_advisory_lock`,
  `pg_try_advisory_lock`, `pg_advisory_unlock`, `pg_advisory_xact_lock`, and
  the `_shared` variants) — naming, registration, and argument-shape
  (single-bigint vs. two-int forms) belong entirely to the pgmicro follow-up
  plan. This plan only builds the engine-side registry and its Rust API.
- **Shared/exclusive lock modes** — the counted registry generalizes to this
  cleanly, but it's real added complexity (a second axis of contention
  rules) with no consumer yet in this plan; add it in a later revision if the
  follow-up plan's SQL surface needs `_shared` variants, rather than building
  it speculatively now.
- **Cross-process locking** — this is an in-process, per-`Database`-instance
  registry (like `PgNotifyHub`), not a durable or cross-process lock; two
  separate OS processes opening the same database file do not see each
  other's locks. Consistent with the fact that this is explicitly a
  cooperative, engine-tracked convenience, not a replacement for real
  transactional locking.
- **Deadlock detection** — a blocking acquire can wait forever (modulo the
  caller's own interrupt/cancellation) if two connections wait on each
  other's held keys. No cycle detection is proposed; this matches the
  "cooperative, not enforced" framing in the feature's own name — the caller
  is responsible for lock ordering, same as any advisory lock in any system.
- **The pgmicro-side follow-up plan itself** — not written here per this
  plan's own directive; see the dependency finding above for what it must
  account for before wiring anything to the wire server.

## Testing

- Unit tests in `core/advisory_lock.rs`: same-connection reentrant
  acquire/release counting (acquire twice, release once → still held;
  release twice → free), cross-connection contention (connection B's
  try-acquire fails while connection A holds the key, succeeds after A
  releases), transaction-scoped auto-release on both commit and rollback
  (not just one), session-scoped locks surviving a transaction boundary
  un-touched, connection-close releasing all session-scoped locks held by
  that connection (mirroring the existing `pg_listen` connection-close
  test), and the two-registry independence from the flagged unverified
  interaction above (a transaction-scoped acquire and a session-scoped
  acquire of the identical key from the same connection do not deadlock or
  interfere, whatever their cross-interaction semantics eventually needs to
  be once psql-verified).
- `cargo test -p turso_core advisory_lock` for the new module.
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`
  and `cargo fmt` per standard workflow.
- No `psql` cross-check applies to this plan directly (no SQL surface is
  added here) — the psql verification obligation for the one flagged-unverified
  behavior (session/transaction-scope interaction on an identical key) and
  for all `pg_advisory_lock`-family output shapes belongs to the pgmicro
  follow-up plan, which must not skip it.

## Migration / compatibility

Pure addition: a new module, two new fields on `Connection` (both empty/no-op
until something acquires a lock), and two new hook call-sites
(`set_tx_state`, `Connection::close()`) that are no-ops when the registries
are empty. No schema/storage format change, no existing behavior changes, no
new external dependency.
