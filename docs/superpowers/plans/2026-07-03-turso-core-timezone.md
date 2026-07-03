# Turso-Core Plan: session-scoped named time zone + IANA zone conversion

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
follow-up pgmicro-side plan is `2026-07-03-pgmicro-timezone-followup.md`.

## Problem

Turso's `datetime()`/`strftime()` family already understands two zone concepts,
both narrow:

- A fixed numeric UTC offset modifier (`'+02:00'`, `'-05:00'`) — no DST, no name.
- The `'localtime'` modifier (`core/functions/datetime.rs:521-560`), which converts
  using `chrono::Local` — the *one* time zone the OS process happens to be
  configured with (via `iana-time-zone`, pulled in transitively by chrono's
  `clock` feature, `core/Cargo.toml:81`). There is no way to ask for any other
  named zone (`America/New_York`, `Asia/Kolkata`, etc.) from SQL — `chrono::Local`
  can only ever represent the single zone the host OS is set to.

This is a real gap independent of any PostgreSQL use case: any Turso user storing
timestamps and wanting to display/convert them in a zone other than the server's
own OS zone (a common requirement — "store in UTC, show in the user's zone") has
no primitive for it today. `chrono` itself (the workspace dependency,
`Cargo.toml:143`, `core/Cargo.toml:81`) does not embed the IANA time zone
database — that's a deliberate design split in the chrono ecosystem, with
`chrono-tz` as the standard companion crate for exactly this. Confirmed via
`Cargo.lock`: `chrono-tz` is not present, transitively or directly, anywhere in
the workspace today.

There is also no concept of a *session-scoped default zone* anywhere in Turso.
`Connection` (`core/connection.rs:159`) already carries one analogous piece of
per-connection session state — `sql_dialect: AtomicSqlDialect`
(`core/connection.rs:232`, get/set at `core/connection.rs:2892-2897`, wired to
`PRAGMA sql_dialect` in `core/translate/pragma.rs:636,1437`) — but nothing
equivalent exists for a time zone. Without it, any feature that wants
"convert to the current session's zone" (rather than requiring every caller to
pass a zone name explicitly) has nowhere to read that state from.

## Design

Two independent, generically-useful additions, following existing patterns:

### 1. New dependency: `chrono-tz`

Add `chrono-tz` to the workspace (`Cargo.toml`) and `core/Cargo.toml`, pinned to a
version compatible with the workspace's `chrono = "0.4.42"`. Justification (per
this project's dependency-hygiene rule — "ask whether the project or the standard
library can already do it first"): neither `chrono` nor the standard library ship
an IANA time zone database; hand-rolling tzdata parsing (DST rules, historical
offset changes, leap seconds policy) is exactly the kind of large, correctness-
critical, already-solved problem this project's dependency principle says to pull
in rather than reinvent. `chrono-tz` is pure Rust (no C toolchain dependency, unlike
some `tz`-family crates), implements `chrono::TimeZone` directly — so it plugs into
the exact same `chrono::LocalResult::{Single,Ambiguous,None}` handling
`core/functions/datetime.rs:521-560` already uses for `'localtime'`, no new
conversion-result-handling pattern needed — and is the de facto standard
companion crate to the chrono version already pinned here.

### 2. Two new zone-conversion scalar functions

Neutral (non-PG) names, added to `core/functions/datetime.rs` alongside the
existing modifier logic, using `chrono_tz::Tz` for lookup:

- `timezone_to_utc(zone_name, local_text) -> utc_text`: parse `local_text` as a
  naive (offset-less) date/time, interpret it as wall-clock time *in* the named
  zone, and return the equivalent UTC instant as text (Turso's standard
  `%Y-%m-%d %H:%M:%f` shape, matching `strftime` output elsewhere in this
  codebase, e.g. `core/schema.rs:684-685`).
- `timezone_from_utc(zone_name, utc_text) -> local_text`: parse `utc_text` as a
  UTC instant, convert to the named zone's local wall-clock representation, and
  return it as text with a trailing numeric offset (`%Y-%m-%d %H:%M:%f%:z`,
  matching the existing `fmt_iso`/`fmt_datetime` offset-formatting style in
  `core/time/internal.rs:120-160`).

Both:
- Return an error (not `NULL`) on an unrecognized zone name
  (`chrono_tz::Tz::from_str` failure) — consistent with this codebase's "assert
  invariants, don't silently fail" principle (Turso Engine Guidelines, Core
  Principles #4): a typo'd zone name silently producing `NULL` would be a wrong-
  result bug indistinguishable from a real absent-data case.
- **Require verification, not assumption**, for DST-transition edge cases: when
  `local_text` in `timezone_to_utc` falls in a "spring forward" gap (nonexistent
  local time) or "fall back" overlap (ambiguous local time), chrono's
  `LocalResult` distinguishes `None`/`Ambiguous` from `Single` — decide and test
  a concrete policy (e.g., error on `None`, pick the earlier offset on
  `Ambiguous`) rather than leaving it to whatever chrono's default happens to do.
  Document the chosen policy in a doc comment and cover both cases with a unit
  test using a real DST-transition date for a real zone (e.g. `America/New_York`
  on its known spring-forward/fall-back dates for the target year).

### 3. Session-scoped default zone: `PRAGMA timezone` + connection-aware getter

- Add `Timezone` to `PragmaName` (`parser/src/ast.rs:1794`), following the exact
  pattern of existing entries (snake_case via `#[strum(serialize_all =
  "snake_case")]`, so `PRAGMA timezone` / `PRAGMA timezone = 'America/New_York'`
  parse without a separate lexer change).
- Add a `time_zone` field to `Connection` (`core/connection.rs:159`), mirroring
  `sql_dialect: AtomicSqlDialect` (`core/connection.rs:232`) — since a zone name
  is a `String`, not a `Copy` enum, use interior mutability appropriate for that
  (e.g. `RefCell<String>` or `RwLock<String>`, implementer's call based on
  existing `Connection` field conventions for non-`Copy` session state). Default
  `"UTC"`.
- Wire `PragmaName::Timezone` in `core/translate/pragma.rs` at both the SET arm
  (mirroring `PragmaName::SqlDialect` at line 636 — validate the assigned zone
  name via `chrono_tz::Tz::from_str` before storing, rejecting unknown names
  loud rather than storing garbage) and the read arm (mirroring line 1437).
- Add a new zero-argument, connection-aware scalar function `session_timezone()`
  that returns the current `Connection.time_zone` value as text. This needs the
  same dispatch shape as `ScalarFunc::PgGetUserById`
  (`core/function.rs:499,669,850,1037,1492`; dispatched with `&program.connection`
  access at `core/vdbe/execute.rs:7145-7150`) — a `PRAGMA` cannot be read from
  inside a SQL expression, but this getter can, which matters for the follow-up
  plan's custom-type `ENCODE`/`DECODE` clauses (pure SQL expressions,
  `core/schema.rs`) that need to reference "the current session's zone" without
  a caller-supplied literal.

### Files touched

- `Cargo.toml`, `core/Cargo.toml` — add `chrono-tz` dependency.
- `core/functions/datetime.rs` — `timezone_to_utc`/`timezone_from_utc`, plus unit
  tests (DST gap/overlap, unknown-zone error, round-trip against a known offset).
- `parser/src/ast.rs` — new `PragmaName::Timezone` variant.
- `core/connection.rs` — new `time_zone` field + get/set accessors (mirroring
  `get_sql_dialect`/`set_sql_dialect`, `core/connection.rs:2892-2897`).
- `core/translate/pragma.rs` — SET/read arms for `PragmaName::Timezone`
  (mirroring `PragmaName::SqlDialect` arms).
- `core/function.rs`, `core/vdbe/execute.rs` — new `ScalarFunc::SessionTimezone`
  variant + dispatch (mirroring `PgGetUserById`).
- Tests: unit tests alongside `timezone_to_utc`/`timezone_from_utc`; an
  integration-level test for `PRAGMA timezone` get/set/reject-unknown, and one
  for `session_timezone()` reflecting a prior `PRAGMA timezone = ...` — likely in
  `core`'s existing pragma/function test modules (implementer to locate the
  established location for `PRAGMA sql_dialect`'s own tests as precedent).

## Explicitly out of scope

- **Any PostgreSQL-specific syntax or semantics** (`AT TIME ZONE`, PG's
  `TimeZone` GUC name, PG's per-type overload direction for `timezone(zone, ts)`
  vs `timezone(zone, tstz)`) — entirely the follow-up plan's concern. This plan
  adds two direction-explicit, neutrally-named functions and one session
  setting; it does not decide how any SQL dialect surfaces them.
- **Changing the existing `timestamptz` custom type** (`core/schema.rs:685`) to
  use these new primitives — that edit belongs to the follow-up plan, since the
  type is itself a PG-flavored instantiation of the generic `CREATE TYPE`
  mechanism (see that file's existing bootstrap list), not a Turso-core concept.
- **Changing `'localtime'`'s existing behavior** — it keeps meaning "the OS
  process's own zone" for backward compatibility; the new session zone setting
  and new functions are additive, not a replacement.
- **A `PRAGMA`-level default zone auto-applied inside `datetime()`/`strftime()`
  modifiers** — e.g. teaching `'localtime'` to consult `PRAGMA timezone` instead
  of `chrono::Local`. That would be a natural next step but changes existing
  function behavior for any current caller relying on `'localtime'` meaning "OS
  zone"; flag as a follow-up if a real workload needs it, don't fold in here.
- **Historical/pre-1970 or far-future date handling nuances** in `chrono-tz`
  beyond what the crate itself supports — out of scope; use the crate's stated
  guarantees as-is.

## Testing

- `cargo test -p turso_core datetime` (or the crate's actual test module path)
  for `timezone_to_utc`/`timezone_from_utc` unit tests, including the DST
  gap/overlap policy tests.
- `cargo test -p turso_core` for the new `PRAGMA timezone` / `session_timezone()`
  integration tests.
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` and
  `cargo fmt` per standard workflow.
- Manual check via `cargo run -q --bin tursodb -- -q`:
  ```sql
  PRAGMA timezone;                                            -- 'UTC' (default)
  PRAGMA timezone = 'America/New_York';
  PRAGMA timezone;                                            -- 'America/New_York'
  SELECT session_timezone();                                  -- 'America/New_York'
  SELECT timezone_to_utc('America/New_York', '2026-07-03 12:00:00');   -- '2026-07-03 16:00:00.000' (EDT, UTC-4)
  SELECT timezone_to_utc('America/New_York', '2026-01-03 12:00:00');   -- '2026-01-03 17:00:00.000' (EST, UTC-5)
  SELECT timezone_from_utc('America/New_York', '2026-07-03 16:00:00'); -- '2026-07-03 12:00:00.000-04:00'
  PRAGMA timezone = 'not_a_real_zone';                         -- error, not silently accepted
  SELECT timezone_to_utc('not_a_real_zone', '2026-01-01 00:00:00');    -- error, not NULL
  ```
  The DST-boundary dates above must be re-checked against the actual year this
  lands in (DST transition dates shift slightly year to year) and against
  `chrono-tz`'s own resolved offsets — do not trust this document's specific
  offsets over the crate's actual output.

## Migration / compatibility

Pure additive feature: new dependency, new PRAGMA (defaults to today's implicit
`'UTC'`-ish behavior most callers already assume), new functions, new
`Connection` field defaulted to a value that changes no existing query's result.
No existing function, PRAGMA, or type behavior changes. No schema/storage format
change, no new public API beyond the two new functions and one new PRAGMA.
