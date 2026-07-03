# Turso-Core Plan: A generic sequence (monotonic counter) object type

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
follow-up pgmicro-side plan (wiring `CREATE SEQUENCE`, `nextval`/`currval`/`setval`,
and `GENERATED ... AS IDENTITY` syntax to the primitive defined here) is a separate
document, not written as part of this plan.

## Problem

Turso has no engine-level object for "a counter that hands out increasing integers
to concurrent callers, independent of transaction boundaries." The closest existing
mechanism is `sqlite_sequence` (`core/translate/schema.rs:1267`, used to back
`AUTOINCREMENT` columns), but it is the wrong shape for this purpose on every axis
that matters:

- **It is transactional.** The row `sqlite_sequence` holds for a table is updated
  through the same B-tree write path as any other row, inside the caller's active
  transaction (`core/translate/alter.rs:156-186` shows it being read/written as an
  ordinary cursor over a `BTreeTable`). Roll back the transaction that inserted the
  row, and the `sqlite_sequence` bump rolls back with it. A generic counter object
  needs the opposite property: advancing it must be visible to every other
  connection immediately and permanently, whether or not the transaction that
  triggered the advance ever commits.
- **It is one-per-table, not a first-class object.** It exists only to make
  `AUTOINCREMENT` work and is keyed by table name. There's no way to create,
  reference, or read one independent of a table's rowid column.
- **It has no notion of "what value did I, this connection, last obtain here."**
  Any caller-facing use of a shared counter needs a way for a caller to ask "what
  did I get last time," scoped to the connection that asked, not the last writer
  from any connection.

This is a real, well-established, dialect-independent SQL feature (a `CREATE
SEQUENCE`-style object exists in the SQL standard, PostgreSQL, Oracle, DB2, and
SQL Server's near-equivalent), and any Turso-based dialect wanting sequence
support needs the same underlying primitive: a named, shared, crash-safe,
concurrency-safe monotonic counter with configurable start/step/bounds, whose
advancement survives rollback of whatever transaction triggered it. Turso should
have this as a general engine feature, not something invented ad hoc inside a
single dialect's translator.

**Acceptance bar:** because Turso's actual purpose (via pgmicro) is reproducing
real database engine behavior, and PostgreSQL sequences are the most rigorously
documented example of this primitive, this plan uses PostgreSQL's sequence
semantics as the concrete correctness spec to match bit-for-bit at the primitive
level — not because any PG syntax is introduced here (none is), but because
"invent a reasonable-sounding generic design" is not the bar; matching a
real, battle-tested implementation's edge cases is. **Every rule below must be
spot-checked against a live `psql` instance before this is considered done** —
do not trust this document's transcription of PG behavior over an actual server's
output if they ever disagree.

## Design

### The object: `Sequence`

A new schema-level object type, `Sequence`, registered in `Schema`
(`core/schema.rs:622`) alongside `tables`, `indexes`, and `triggers` — following
the existing field pattern (`pub tables: HashMap<String, Arc<Table>>` at
`core/schema.rs:623`) rather than inventing a new registration mechanism:

```rust
pub sequences: HashMap<String, Arc<Sequence>>,
```

Each `Sequence` carries the standard set of properties every real-world
implementation of this primitive exposes: current value, start value, increment
step (may be negative — a sequence can count down), minimum and maximum bounds,
and a cycle flag (whether hitting a bound wraps to the other end or errors). These
are object properties, not statement syntax — how a dialect's grammar sets them at
creation time is out of scope for this plan.

The engine-level operations a `Sequence` must support (generic names used
throughout this plan; a dialect's function/statement names for these are the
follow-up plan's concern):

- `advance(&self) -> Result<i64>` — atomically compute and return the next value,
  applying step/bounds/cycle, and durably persist the new current value.
- `last_advanced_value(&self, connection_id: u64) -> Option<i64>` — return the
  last value *this connection* obtained via `advance`, or `None` if this
  connection has never called `advance` on this sequence.
- `set(&self, value: i64, mark_as_advanced: bool)` — force the current value.
  `mark_as_advanced` controls whether the *next* `advance()` call returns `value`
  itself (if forced value was not yet "consumed") or the value after it (if it
  was) — see **Design point 4** below for why this needs to be a distinct flag
  rather than folded into `value`.

### Design point 1: Advancement is non-transactional

**The core correctness requirement, and the one with no precedent anywhere in
this codebase today.** Real PostgreSQL sequences are explicitly documented as
non-transactional: `nextval()`'s effect is never undone by a `ROLLBACK`, even
though the call that invoked it happened inside a transaction that rolled back.
Verified by reasoning from PG's documented behavior (must still be spot-checked
against live `psql` per the caveat above):

```sql
BEGIN;
SELECT nextval('s');  -- returns, say, 5
ROLLBACK;
SELECT nextval('s');  -- returns 6, NOT 5 again
```

I confirmed by grep that **no existing Turso mechanism does this today.**
`core/storage/pager.rs:2670`'s `begin_write_tx` ties every page write to the
calling connection's transaction; there is no "write this page and make it
durable right now, regardless of what the caller's transaction does next"
primitive anywhere in `core/storage/`. This is genuinely new engineering ground,
not a rewiring of something that already exists — flag this explicitly to
whoever picks up implementation, rather than assuming a good pattern to copy
already exists.

The implementation needs a write path for a `Sequence`'s current-value update that
commits independently of the calling connection's transaction — effectively a
small internal auto-committing sub-transaction scoped to just that one counter's
storage, analogous to how PostgreSQL itself implements this (sequence access
bypasses normal MVCC snapshot rules and writes its backing page in place,
immediately, outside the calling transaction's 2PL). The exact mechanism (a
dedicated small WAL-adjacent write path vs. a special-cased single-page commit
inside the existing pager) is an implementation decision for whoever picks this
up, but the requirement — durable, un-rollback-able state change, decoupled from
the caller's transaction lifecycle — is not negotiable; it is the entire point of
the feature.

**Test case:** start a write transaction, call `advance()`, roll the transaction
back, call `advance()` again from the same connection — the second call must
return the value after the first, not the same value again. Repeat from a
*different* connection after the first connection's rollback to confirm the
advance is visible database-wide, not just connection-local.

### Design point 2: Crash recovery may skip forward, never backward, never repeat

A corollary of point 1: if every single `advance()` call had to fsync a WAL frame
before returning, sequences would be as slow as a full transactional write per
call, which defeats their purpose (real workloads call sequences at high
frequency — one per inserted row, in the common case). Real PostgreSQL's actual
solution (documented, and must be verified against real behavior, not assumed):
it does **not** guarantee gapless output across a crash. PG pre-logs a batch of
future values (durability is amortized across several `advance`-equivalents
before the next durable write), so a crash between durable log points can cause
the post-recovery counter to jump *ahead* past some values that were handed out
in-memory but never made durable — those values are simply skipped, never
reissued. What must never happen, under any circumstances including a crash mid
`advance()`, is the same value being handed out twice.

This plan adopts the same tradeoff: `Sequence::advance()` may amortize durability
across a small batch (implementation detail: how large a batch, left to whoever
implements this — smaller batches lose fewer values on crash but cost more I/O;
this number is not a correctness property to match, only the "gaps allowed,
repeats forbidden, monotonic" invariant is).

**Test case:** cannot be exercised as an ordinary unit test (requires simulating
a crash mid-batch); this needs either a fault-injection test using Turso's
existing `testing/simulator/` fault-injection harness (kill the process after
`advance()` returns but before the next durability checkpoint, restart, call
`advance()` again, assert the new value is strictly greater than the last one
returned pre-crash — gap is acceptable, repeat or regression is not) or, at
minimum, an explicit code comment plus a tracked follow-up if simulator coverage
isn't feasible in the first pass. **Verify against live psql**: confirm this gap-
after-crash behavior is really documented PG behavior and not a misremembering —
check the current PostgreSQL docs section on sequences (`nextval`) before relying
on this design point.

### Design point 3: Per-connection memory of the last value obtained

`last_advanced_value(connection_id)` must answer "what did *this* connection last
get from *this* sequence," and must distinguish "this connection called
`advance()` and got some value" from "this connection has never called
`advance()` here" — the second case is a distinct, must-be-representable state
(`None`/error), not indistinguishable from having received the sequence's start
value.

Grounding for where this lives: `core/connection.rs:169` already has exactly this
shape of state — `pub(super) last_insert_rowid: AtomicI64` is connection-scoped,
in-memory, and updated outside the normal MVCC/schema path. The difference here
is cardinality: `last_insert_rowid` is one scalar because a connection only ever
has one "most recent insert," but a connection can call `advance()` against many
different `Sequence` objects, so this needs a per-connection map (sequence
identity → last value), not a single `AtomicI64`. This map lives on `Connection`,
guarded the same way other per-connection mutable state already is in that file
— not in `Schema`, since it must NOT be visible to other connections (unlike the
counter's actual current value, which is shared database-wide per point 1).

**Test case:** two connections, same sequence. Connection A calls `advance()`
(gets 1), connection B calls `advance()` (gets 2). Connection A's
`last_advanced_value` must return 1 (not 2, even though 2 is the sequence's
globally-current value) — read-your-own-writes, not a shared cursor. A brand new
connection C that has never called `advance()` on this sequence must get `None`,
distinctly from a connection that called `advance()` and happened to get the
start value. **Verify against live psql**: PG's real behavior for a fresh session
calling `currval()` before ever calling `nextval()` in that session is
**documented to error**, not to return NULL/0 — confirm this exact failure mode
(error vs. some sentinel value) before treating "no prior call" as anything other
than an error condition the caller must handle.

### Design point 4: `set()` needs two independent effects, not one

Real PostgreSQL's `setval()` takes an optional second argument (`is_called`)
specifically because "set the counter to X" and "should the *next* advance return
X itself, or X+1" are two independent decisions a caller needs to make
separately — collapsing them into a single value (e.g. always "next advance
returns X+1") loses the ability to force the *exact* next output of `advance()`,
which is a real, used capability (e.g. resetting a sequence to continue exactly
where an external data import left off). This is why `Sequence::set` above takes
`(value, mark_as_advanced)` as two parameters rather than one. Like point 1, this
is a non-transactional operation — the same rollback-survival requirement
applies to `set()` as to `advance()`.

**Test case:** `set(100, mark_as_advanced=true)` then `advance()` must return
101. `set(100, mark_as_advanced=false)` then `advance()` must return 100 itself.
**Verify against live psql**: confirm the exact two-argument semantics and which
boolean value maps to which behavior — get this backwards and every consumer of
the primitive silently produces off-by-one sequences.

### Design point 5: Concurrent advancement must not serialize with unrelated writers

Multiple connections calling `advance()` on the same `Sequence` concurrently must
never receive the same value, and — just as importantly for real-world
usability — calling `advance()` must not block on or get blocked by unrelated
table writes elsewhere in the database. Real PostgreSQL sequences are
specifically designed so that concurrent `nextval()` calls (even from
transactions that never commit) don't create write contention with ordinary
table DML; this is a direct consequence of point 1 (non-transactional, its own
lightweight locking) rather than a separate mechanism. The locking strategy for
a `Sequence`'s current-value update must be scoped to that individual sequence
(e.g. a per-`Sequence` mutex or atomic compare-and-swap over its in-memory
current-value cache, backed by the durable write path from point 1), not routed
through the general MVCC/B-tree write-transaction machinery that ordinary table
writes use — routing through the general path would reintroduce exactly the
serialization-with-unrelated-writers problem this point exists to avoid.

**Test case:** spawn N concurrent connections each calling `advance()` M times
on the same sequence; collect all returned values; assert the multiset of
returned values is exactly `{start, start+step, ..., start+(N*M-1)*step}` with no
duplicates and no gaps (no crash involved here, so point 2's gap-allowance
does not apply — under normal concurrent operation without a crash, output must
be gapless and duplicate-free). Separately, assert that a long-running write
transaction on an unrelated table does not block a concurrent `advance()` call
on a `Sequence` (and vice versa) — e.g. hold a write lock on table `t` in one
connection and confirm `advance()` on `Sequence` `s` from another connection
still completes without waiting for the first transaction to end.

### Storage and catalog representation

A `Sequence`'s existence (name, properties) needs to be recorded durably and
recovered on database reopen, same as a table or index. Whether this is a new
dedicated system table (analogous to how `sqlite_sequence` already exists as a
real table, `core/translate/schema.rs:1267`) or a new row-kind inside the
existing schema-defining table is an implementation decision left to whoever
picks this up — but whichever is chosen, the *current value* of each `Sequence`
must NOT be stored the same way its *definition* is stored, because the
definition (name, start, step, bounds, cycle) is ordinary transactional schema
metadata (created via a normal CREATE-equivalent, rolled back like any other
DDL if the creating transaction rolls back), while the current value is the
non-transactional part described in points 1–2. Conflating the two into one
row updated through the normal B-tree path would silently reintroduce the exact
transactional-rollback bug this plan exists to avoid.

## Files touched

- `core/schema.rs` — new `Sequence` struct and `Schema::sequences` map, following
  the existing `tables`/`indexes`/`triggers` registration pattern
  (`core/schema.rs:622-640`).
- `core/connection.rs` — new per-connection map for `last_advanced_value`
  (point 3), alongside the existing `last_insert_rowid: AtomicI64`
  (`core/connection.rs:169`) as the closest existing analog.
- `core/storage/pager.rs` and/or `core/storage/wal.rs` — the new
  non-transactional, crash-tolerant-with-gaps durable write path (points 1–2).
  This is the largest and least precedented piece of work in this plan; budget
  design review time here specifically, since no existing code path does
  anything like it today (confirmed by grep — see point 1).
- A new module, likely `core/sequence.rs` — `Sequence`'s `advance`/`set` logic,
  in-memory current-value cache, and the per-sequence locking from point 5.
- Tests: unit tests for points 3–5 (ordinary `cargo test`), a fault-injection
  test for point 2 using `testing/simulator/` if feasible, and an explicit
  documented gap if not.

## Explicitly out of scope

- **Any SQL syntax**: no `CREATE SEQUENCE`, no `nextval`/`currval`/`setval`
  function names, no `GENERATED ... AS IDENTITY` column syntax, no grammar or
  parser changes of any kind. This plan defines the engine-level primitive only;
  wiring PG syntax to it (both `CREATE SEQUENCE`/`nextval`/`currval`/`setval`
  *and* `GENERATED ... AS IDENTITY`, which share this same backing primitive)
  is a separate, not-yet-written pgmicro-side follow-up plan, per the two-plan
  rule — do not add it here.
- **Any dialect-specific default naming convention** (e.g. PG's
  `<table>_<column>_seq` implicit sequence naming for serial/identity columns):
  a dialect concern, not a primitive concern.
- **Performance tuning of the batching size in point 2**: a reasonable default
  is needed for the primitive to function, but tuning it against real workload
  numbers is a follow-up, not a blocker for correctness.
- **Distributed/replicated sequence behavior**: out of scope; this plan assumes
  a single-node Turso database, consistent with the rest of the engine's current
  scope.

## Testing

- `cargo test -p turso_core sequence` (or the crate's actual module path once
  named) for the new unit tests (points 3, 4, 5).
- A `testing/simulator/` fault-injection scenario for point 2 (crash mid-batch),
  if the harness supports killing/restarting mid-operation at this granularity;
  otherwise, document the gap explicitly rather than silently skipping it.
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` and
  `cargo fmt` per standard workflow.
- Manual verification via `cargo run -q --bin tursodb -- -q` is not meaningful
  here in isolation, since this plan adds no SQL surface — meaningful manual
  verification only becomes possible once the pgmicro follow-up plan wires SQL
  syntax to this primitive. At that point, **every semantic rule in this
  document must be re-confirmed against a live `psql` instance**, not just
  assumed correct because this document says so.

## Migration / compatibility

Pure addition: a new optional schema object type that does not exist unless
explicitly created, and no existing table/index/trigger code path changes
behavior. Existing database files with no sequences remain fully compatible —
`Schema::sequences` is simply empty for any database that never creates one. No
existing public API changes. The one point requiring care during implementation,
not migration: whatever on-disk representation is chosen for a `Sequence`'s
current value (point on storage/catalog representation above) needs its own
forward-compatible format decision now, since it's new durable file-format
surface — but it does not need to preserve compatibility with any *existing*
on-disk data, since no prior Turso version ever wrote this data.
