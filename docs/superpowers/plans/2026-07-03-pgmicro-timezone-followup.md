# pgmicro Plan: `TIMESTAMPTZ` real timezone semantics, `SET TimeZone`, `AT TIME ZONE`

**Depends on:** `2026-07-03-turso-core-timezone.md` landing first (adds
`chrono-tz`, `PRAGMA timezone`, `session_timezone()`, `timezone_to_utc()`/
`timezone_from_utc()`). Do not start this plan before that one merges.

**Covers** the "full `TIMESTAMPTZ` timezone semantics (`AT TIME ZONE`, session
`TimeZone` GUC, offset-aware storage)" item flagged as a two-plan candidate in
`2026-07-02-pgmicro-fixes.md:7197`.

## Problem, precisely

Today's `timestamptz` custom type (`core/schema.rs:685`):

```
CREATE TYPE timestamptz(value text) BASE text
  ENCODE CASE WHEN value IS NULL THEN NULL
             WHEN datetime(value) IS NULL THEN RAISE(ABORT, 'invalid timestamp value')
             ELSE strftime('%Y-%m-%d %H:%M:%f', value) END
  DECODE value
  OPERATOR '<'
```

is byte-for-byte identical to the plain `timestamp` type one line above it
(`core/schema.rs:684`) — it has never had any timezone-specific behavior. SQLite's
`datetime()`/`strftime()` do parse a trailing numeric offset on *input*
(`'2026-01-01 10:00:00+05:00'`) and convert to UTC before formatting, but:

1. **Output never carries an offset** — `strftime('%Y-%m-%d %H:%M:%f', ...)`
   with no zone modifier always renders the UTC-converted instant with no
   offset suffix. Real PostgreSQL always renders `timestamptz` text with a
   trailing offset appropriate to the session's `TimeZone` (default the server's
   own zone, e.g. `2026-01-01 05:00:00-05`).
2. **Offset-less input is silently assumed to already be UTC** — there is no
   session zone to interpret it against. Real PostgreSQL interprets a
   zone-less `timestamptz` input literal using the session `TimeZone` GUC, not
   a hardcoded UTC assumption.
3. **`SET TimeZone = '...'` currently does nothing observable.** It parses (via
   `try_prepare_pg()`'s `SET`→`PRAGMA` passthrough, `core/connection.rs`), but no
   PRAGMA exists to receive it — and once Task C2 (`2026-07-02-pgmicro-fixes.md`,
   `is_pg_noop_guc`) lands, `timezone` is explicitly on the no-op GUC allowlist,
   meaning `SET TimeZone = 'America/New_York'` will be *accepted and silently
   discarded* rather than erroring. **This plan must remove `"timezone"` from
   that no-op list** (or Task C2 must land after this plan, whichever lands
   second needs to reconcile) once real behavior exists — leaving both in place
   would silently ignore every `SET TimeZone` a real client sends, which is worse
   than today's honest error.
4. **No `AT TIME ZONE` support at all** — confirmed via grep: zero handling in
   `parser_pg/src/translator.rs`. PostgreSQL's own grammar desugars
   `expr AT TIME ZONE zone` into a function call to `pg_catalog.timezone(zone,
   expr)` (two-arg, direction depends on whether `expr`'s *static* type is
   `timestamp` or `timestamptz` — PG overloads the function by argument type).
   **This needs verification, not assumption**: dump the actual `pg_query`
   protobuf tree for `SELECT ts AT TIME ZONE 'UTC'` (via
   `turso_parser_pg::parse(...)` and `println!("{:#?}", ...)`, per this
   codebase's own documented debugging method,
   `CLAUDE.md`/"Debugging PG translation issues") before writing the translator
   arm — do not implement against this document's description of PG's grammar
   without confirming the actual node shape pg_query hands back.

## Design

### 1. Wire `timestamptz` to real session-zone-aware conversion

Rewrite the `timestamptz` `CREATE TYPE` row in `core/schema.rs` (only this row —
`timestamp`, `date`, `time` stay untouched, they have no zone concept in PG
either) to:

- **ENCODE** (on write): if the input text has an explicit offset, convert to
  UTC as today (`datetime(value)` already does this correctly — keep it).
  If it has no explicit offset, it must be interpreted as wall-clock time in
  the *session's* zone rather than assumed-UTC. This is where the
  `session_timezone()` getter from the core plan is needed inside the `ENCODE`
  expression: call `timezone_to_utc(session_timezone(), value)` when `value`
  has no offset, falling back to the existing `datetime(value)` conversion when
  it does. (Exact `CASE`-expression branching between "has offset" vs "doesn't"
  needs a cheap-to-evaluate SQL test — check what `date`/`datetime`-family
  helper already exists in this codebase for that, e.g. reusing whatever
  `core/functions/datetime.rs`'s existing modifier parser uses to detect a
  trailing offset, rather than hand-rolling a new regex/LIKE check in SQL.)
- **DECODE** (on read): today just returns `value` unchanged (already
  UTC-normalized, no offset shown). Change to
  `timezone_from_utc(session_timezone(), value)` so reads always render with
  the session zone's current offset — matching PG's actual `timestamptz` output
  behavior.
- Storage stays UTC-normalized text (unchanged wire format / no migration for
  already-stored rows — see Migration section).

**Requires verification against real `psql`, not just this document**: PG's
actual `timestamptz` input-interpretation and output-rendering rules have a few
documented edge cases (e.g. how `TimeZone` interacts with literals that specify
`Z` vs an explicit offset vs nothing) — spot-check each case in the test list
below against a live `psql` before considering this done, per the same standard
the JSON containment plan pair already established for this project.

### 2. `SET TimeZone` / `SHOW TimeZone` → real `PRAGMA timezone`

In `try_prepare_pg()` / `core/pg_dispatch.rs`'s SET/SHOW handling: map PG's
`TimeZone` GUC name (case-insensitive, per existing GUC handling) to the new
`PRAGMA timezone` from the core plan, instead of (post-Task-C2) treating it as a
no-op. Concretely: remove `"timezone"` from `is_pg_noop_guc`'s list (or, if
Task C2 hasn't landed yet when this plan starts, simply never add it) and add an
explicit `SET`/`SHOW` name-rewrite for `TimeZone` → `PRAGMA timezone`, mirroring
how `search_path` already gets special-cased above the generic PRAGMA
passthrough (`core/pg_dispatch.rs`'s existing `set_stmt.name == "search_path"`
arm). Validate the assigned zone name the same way the core plan's `PRAGMA
timezone` SET arm already does (reject unknown zones loud) — no new validation
needed here, just routing.

### 3. `AT TIME ZONE` translation

After confirming the actual pg_query node shape (see Problem #4's verification
requirement): add a `translate_*` arm that maps it to a call to
`timezone_from_utc(zone, expr)` (the common case: displaying a `timestamptz`
value in an explicit zone — this is what the vast majority of real `AT TIME
ZONE` usage is, and it's the direction pgmicro's `timestamptz` storage format
already supports without ambiguity, since storage is always UTC-normalized).

**Known, named limitation — do not attempt to fully fix here**: PG's `timezone()`
overloads by the *static* type of its second argument (`timestamp` → produces a
`timestamptz`, interpreting the naive value as being in the given zone;
`timestamptz` → produces a `timestamp`, converting to the given zone's wall
clock). The translator has no static type information at translate time
(the same limitation already flagged for Task B16's `@>`/`<@` dispatch,
`2026-07-02-pgmicro-fixes.md:3463`, and Task B12/B17's constraint-name
resolution, `translator.rs:3123`) — and confirmed via `core/types.rs`'s
`TextSubtype` enum (`Text`, `Json` only) that there is no runtime tag
distinguishing a `timestamp`-typed text value from a `timestamptz`-typed one the
way JSON's `TextSubtype::Json` distinguished JSON text from array text in the
JSON containment plan. Closing this gap for real needs either the
schema-lookup-hook architecture change (out of scope, same citation as B12/B17)
or a new `TextSubtype` variant (a further Turso-core change, its own two-plan
candidate, not bundled here). **Document this as a doc comment at the
translation arm**, matching the precedent set by Task B18/B19 (CHAR/TIMETZ
doc-only notes) and the JSON followup plan's `&&`-on-JSONB gap — do not leave it
silently wrong with no explanation. Default to the `timezone_from_utc` direction
since it is unambiguous for pgmicro's actual `timestamptz` column type (columns
declared `timestamptz` are the overwhelmingly common case for `AT TIME ZONE`),
and reject (translate-time error, not silent wrong-direction conversion) if a
future task needs the `timestamp`-typed direction before the schema-lookup hook
exists.

### Files touched

- `core/schema.rs` — rewrite the `timestamptz` `CREATE TYPE` row only.
- `core/pg_dispatch.rs` — `TimeZone` GUC name → `PRAGMA timezone` routing;
  remove/never-add `"timezone"` from `is_pg_noop_guc` (coordinate with whichever
  of Task C2 / this plan lands second).
- `parser_pg/src/translator.rs` — new `AT TIME ZONE` translation arm, with the
  documented direction-ambiguity limitation as a doc comment.
- Tests:
  - `tests/integration/postgres/` — `timestamptz` round-trip with explicit
    offset, without offset (interpreted via session zone), `SET TimeZone`
    changing subsequent read rendering, `SHOW TimeZone` reflecting a prior SET,
    `AT TIME ZONE` translation end-to-end. Follow the existing test-file
    pattern established by `tests/integration/postgres/interval.rs` (`TempDatabase`,
    `PRAGMA sql_dialect = postgres`, `turso_macros::test(mvcc)`).
  - `parser_pg/tests/` — `AT TIME ZONE` parse/translate unit test.

## Explicitly out of scope

- **The `timestamp` (non-tz) → `timestamptz` direction of `AT TIME ZONE`** — see
  Design §3's documented limitation.
- **Fixing the underlying translator schema-blindness** — cited but not
  addressed here, same as every other plan that has hit this limitation
  (B12/B17, B16 followup).
- **A new `TextSubtype` variant to distinguish `timestamp` vs `timestamptz` at
  the value level** — a real potential fix for §3's ambiguity, but its own
  Turso-core two-plan candidate; not bundled into this plan.
- **`timestamptz` arithmetic with `INTERVAL`** (`timestamptz_col + interval '1
  day'`) — neither `timestamp` nor `timestamptz` currently declare any `OPERATOR
  '+'`/`'-'` in `core/schema.rs`'s bootstrap list (unlike `interval`, `numeric`,
  `money`, which do). That gap predates this plan and is orthogonal to timezone
  correctness specifically — flag separately, don't fold in here.
- **Historical PostgreSQL `TimeZone` GUC value formats** other than IANA zone
  names (PG also accepts POSIX TZ strings and a handful of legacy abbreviations
  like `PST8PDT`) — out of scope; `chrono-tz` only resolves IANA names, and this
  plan only wires those through. Document the narrower acceptance as a gap if a
  real client sends a POSIX-format value.

## Testing

- `cargo test -p turso_parser_pg` — new `AT TIME ZONE` parse/translate test.
- `cargo test -p core_tester --test integration_tests integration::postgres` —
  new `timestamptz`/`SET TimeZone`/`AT TIME ZONE` end-to-end tests.
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`.
- Manual check via `cargo run -p pgmicro -- :memory:`, **cross-checked against a
  real `psql`** for every case:
  ```sql
  SET TimeZone = 'America/New_York';
  CREATE TABLE t (id INTEGER, ts TIMESTAMPTZ);
  INSERT INTO t VALUES (1, '2026-07-03 12:00:00');        -- interpreted as EDT (session zone)
  INSERT INTO t VALUES (2, '2026-07-03 12:00:00+00');     -- interpreted as explicit UTC
  SELECT ts FROM t ORDER BY id;
  -- row 1: '2026-07-03 12:00:00-04'  (stored as UTC 16:00, displayed in session zone)
  -- row 2: '2026-07-03 08:00:00-04'  (stored as UTC 12:00, displayed in session zone)
  SELECT ts AT TIME ZONE 'UTC' FROM t WHERE id = 1;        -- '2026-07-03 16:00:00'
  SHOW TimeZone;                                           -- 'America/New_York'
  ```

## Migration / compatibility

**Existing stored `timestamptz` data is unaffected** — storage format stays
UTC-normalized text, unchanged. The *display* (`DECODE`) and *offset-less-input
interpretation* (`ENCODE`) behavior changes: previously offset-less input was
silently treated as UTC and output silently showed no offset; now both honor
`PRAGMA timezone` (defaulting to `'UTC'`, per the core plan's default — so a
fresh connection with no `SET TimeZone` sees byte-identical behavior to today).
Existing queries that assumed today's UTC-only behavior only change result
formatting if a caller actually issues `SET TimeZone` to something other than
`UTC` — which today does nothing, so no currently-passing query's *behavior*
regresses; only its previously-silent-wrong output for a `SET TimeZone` caller
becomes correct.
