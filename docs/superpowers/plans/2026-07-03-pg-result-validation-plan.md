# Plan: Validate pgmicro results against real PostgreSQL, exactly

## Mandate

Implementation inside pgmicro/Turso is free to differ arbitrarily from
PostgreSQL's internals (different storage engine, different bytecode VM,
different type representations). **Output must match exactly**: same rows,
same values, same column types/names, same error class (SQLSTATE) for the
same input SQL. This plan's job is to make "matches exactly" a thing that is
mechanically checked against a real PostgreSQL server, not asserted by a
human who checked once by hand and wrote the result down as a Rust literal.

## Grounding: what exists today (verified, not assumed)

A full read-only audit of the codebase's test/validation infrastructure
found **zero live differential-testing-against-real-PostgreSQL
infrastructure anywhere in this repository.** Specifically:

- `scripts/diff.sh` compares **sqlite3 vs tursodb only** — no `psql`, no
  `PGHOST`, nothing Postgres-related. It also always exits 0 regardless of
  match/mismatch (a documented quirk — human reads the diff).
- `testing/differential-oracle/` (`fuzzer/`, `sql_gen/`, `sql_gen_prop/`,
  `sql_gen_macros/`) is a SQLite-vs-tursodb fuzzing oracle.
  `fuzzer/oracle.rs:1-5`'s own doc comment: *"The primary oracle is the
  `DifferentialOracle` which compares Turso results against SQLite."* No
  Postgres dialect awareness. Its SQL-generation grammar (`sql_gen`,
  `sql_gen_prop`) emits SQLite-flavored types/functions/PRAGMAs — reusable
  as a **harness skeleton** (generation loop, schema tracking, oracle-check
  plumbing, coverage reporting), not reusable as a **grammar** (would need
  rewriting for PG syntax, not extending).
- `tests/integration/postgres/` (9 files, ~240 `#[test]` functions —
  `dialect.rs` 110, `catalog.rs` 55, `copy.rs` 35, `interval.rs` 9,
  `parse_edge_cases.rs` 9, `domain.rs` 9, `update_from.rs` 7, `table.rs` 3,
  `type_aliases.rs` 3) and `parser_pg/tests/` (`parse_valid.rs` 104,
  `parse_invalid.rs` 34) are **100% hardcoded-Rust-literal-expectation**
  tests, verified by reading actual test bodies (e.g. `interval.rs:5-58`):
  run SQL in-process, assert against a literal array someone presumably
  checked against real Postgres once, by hand, at write time. Zero of these
  invoke a Postgres client or `psql` subprocess.
- `.github/workflows/pgmicro-ci.yml`'s `test-postgres` job just runs the
  above Rust tests. No `services:` block, no Postgres Docker image,
  anywhere in any workflow in the repo (grepped all of
  `.github/workflows/*.yml`).
- No Postgres client dependency exists in the workspace
  (`tokio-postgres`/`postgres`/`sqlx`/`testcontainers` — none present).
  Only `postgres-protocol`/`postgres-types` (wire-protocol *encoding*
  crates used server-side by `pgwire` in `cli/pg_server.rs` — not a client
  for connecting outward to a real server).
- `git log --all` has no abandoned prior attempt at this.

The one genuine asset already in place: `cli/pg_server.rs:1719-1775`
(`limbo_error_to_pg`) already maps every internal `LimboError` variant to a
real PG SQLSTATE code, with an existing test
(`test_limbo_error_sqlstate_mapping`, line ~1875) asserting specific codes.
This gives error-case comparison a structured axis (5-char SQLSTATE) instead
of forcing fragile human-readable-text matching — a real advantage
`scripts/diff.sh` doesn't have on either of its sides.

**Conclusion: this must be built from scratch.** It can reuse
`testing/differential-oracle`'s harness *pattern* and `scripts/diff.sh`'s
"shell out, don't add a client dependency" *approach*, but there is no
existing code to extend.

## Design

### 1. Two independent choices, decided separately (not blended)

This plan initially forked into two competing drafts — one using a
`tokio-postgres` client crate plus in-process pgmicro comparison, one using
`psql` subprocess on both sides. Per this project's "surface conflicts,
don't average them" rule, these are resolved as two **separate** decisions,
each on its own merits, rather than split the difference:

**Reaching real PostgreSQL: `psql` subprocess, not a Rust client crate.**
Per this project's dependency-hygiene rule ("every dependency is permanent
code you do not control — ask whether the project can already do it"): do
not add `tokio-postgres`/`postgres`/`sqlx` to the workspace as a new
dependency. Shell out to `psql` exactly as `scripts/diff.sh` already shells
out to `sqlite3` — same precedent, same justification. A Rust client
library also has its own type-decoding/formatting opinions that could
silently diverge from what `psql` itself shows a human, and `psql`'s output
is the actual ground truth this project cares about matching.

**Reaching pgmicro: in-process Rust API as the primary tier, not the wire
protocol.** This matches pgmicro's own documented convention (this file's
own CLAUDE.md, Core Principle 6: *"Test with the REPL first, psql second...
Primary testing uses `cargo run -p pgmicro` or the Rust integration tests.
Wire protocol testing via psql is verification, not the primary test
path."*) — reversing this for a brand-new test harness would fork an
established codebase convention silently, which the project's own rules
prohibit (match conventions, or surface a reason to change them; don't fork
silently). In-process is also faster (no server startup + TCP round trip
per fixture) and reuses the exact connection/dialect setup pattern already
used throughout `tests/integration/postgres/`
(`PRAGMA sql_dialect = 'postgres'` on a `TempDatabase`).

**Secondary tier: a smaller wire-protocol suite.** The in-process comparison
above cannot see bugs that live specifically in `cli/pg_server.rs`'s wire
encoding (e.g. `encode_value`'s numeric/timestamp text formatting diverging
from the in-process `Value` even when the in-process value is itself
correct). Keep a second, smaller fixture set that drives `psql` against
`cargo run -p pgmicro -- :memory: --server ...` specifically to catch this
class of bug — real Postgres side is still reached via `psql` in this tier
too, so the comparison stays psql-vs-psql (symmetric formatting) for this
subset. This tier is deliberately smaller: it exists to validate the wire
encoder, not to re-run the full correctness corpus twice.

### 2. What "matches exactly" means — canonicalization rules (decide these
   explicitly, don't leave them implicit)

A naive byte-for-byte diff of `psql` output will produce false positives on
things that are legitimately nondeterministic or cosmetic, and false
negatives are unacceptable (silently treating a real mismatch as
"formatting"). Every canonicalization rule below must be justified by a
verified real-PG behavior, not assumed:

- **Row order**: PostgreSQL makes **no ordering guarantee** absent
  `ORDER BY`. The harness must either (a) require every corpus query to
  include a deterministic `ORDER BY`, or (b) sort both result sets
  identically before comparing when no `ORDER BY` is present. Prefer (a) —
  it's the only way to also validate that pgmicro's *default* scan order
  isn't being silently relied upon elsewhere. Flag any corpus query that
  can't be given a deterministic ORDER BY (e.g. testing `SELECT DISTINCT`
  set semantics) as needing (b) explicitly, not silently.
- **Nondeterministic values must be excluded from equality, not
  canonicalized**: `now()`/`CURRENT_TIMESTAMP`/`clock_timestamp()`,
  sequence-derived values (`nextval`, `SERIAL`/`GENERATED` defaults) whose
  exact number depends on execution order/session history, and OIDs
  (`pg_class.oid` etc. — real Postgres assigns these from its own catalog
  counter, pgmicro's will never match numerically). The corpus must mark
  each query's nondeterministic columns explicitly (e.g. a per-fixture
  `ignore_columns: [...]` field) rather than the harness silently
  discovering "these never match" and excluding them — that would hide real
  bugs where a column that *should* be deterministic isn't.
  the columns.
- **Numeric/text formatting**: confirm (via psql against real PG,
  documented per query) exact expected formatting for types whose text
  representation has real subtlety — `interval` (`tests/integration/postgres/interval.rs`
  already found real edge cases here), `numeric` trailing zeros,
  `timestamptz` display depending on session `TimeZone`. Do not assume
  pgmicro's current formatting is right just because a hardcoded test
  passes — that hardcoded value is exactly the kind of "checked once by
  hand" claim this whole plan exists to stop trusting blindly. Re-verify
  every existing hardcoded-expectation test's literal against live `psql`
  as part of migrating it into this harness (see Phase 1).
- **Error comparison uses SQLSTATE, not message text**: reuse
  `limbo_error_to_pg`'s existing mapping. A corpus query expected to error
  asserts the 5-char SQLSTATE code only. Human-readable message text is
  never part of the pass/fail comparison (PG's own message wording isn't
  even stable across its minor versions) — but the harness should still
  *display* both message texts side by side on any run for human
  inspection, since a wildly different message on a matching SQLSTATE can
  still indicate the wrong root cause was hit.
- **Column metadata**: name is an exact match always. Type OID is an exact
  match wherever pgmicro claims OID-level fidelity
  (`core/pg_catalog.rs`'s `sqlite_type_to_pg_oid()`) — if a type has no
  OID-faithful mapping yet, that is a finding to report explicitly, not
  something the harness silently skips over.

### 3. Harness architecture

```
testing/pg-differential/            # new crate, Rust (needed regardless of
│                                   # psql-subprocess choice above, since the
│                                   # in-process tier drives pgmicro's Rust
│                                   # API directly — a shell script cannot
│                                   # do that half of the comparison)
├── primary/                  # Tier 1: in-process pgmicro (TempDatabase +
│                              # PRAGMA sql_dialect='postgres', same pattern
│                              # as tests/integration/postgres/) vs. `psql`
│                              # subprocess against real Postgres. Canonicalize
│                              # pgmicro's in-process Value to PG text format
│                              # using cli/pg_server.rs's encode_value *rules*
│                              # (reused as a library fn, not via the wire
│                              # path) so both sides land in the same text
│                              # representation before diffing.
├── wire/                     # Tier 2: psql-vs-psql, real Postgres vs.
│                              # `cargo run -p pgmicro -- :memory: --server`.
│                              # Small, targeted at encode_value/wire-encoding
│                              # fidelity only — not a full corpus re-run.
├── corpus/
│   ├── shipped/              # Phase 1: fixtures mirroring already-shipped
│   │                         # Workstream E/B fixes
│   ├── pending/               # Phase 2: fixtures for the 8 two-plan
│   │                         # Turso-core features, added as each lands
│   └── fuzz-seeds/           # Phase 3: seed corpus for the property-based
│                             # generator
└── README.md                 # setup instructions: requires a reachable
                                # real Postgres (local docker run or CI
                                # service), mirrors diff.sh's "requires
                                # sqlite3 on PATH" documented quirk; also
                                # states the exact pinned target PG version
                                # (see Open Questions)
```

Each corpus fixture is a small file: setup SQL (DDL + seed data), the query
under test, expected-SQLSTATE-if-error, and `ignore_columns` if applicable.
The Tier 1 (primary) runner:
1. Runs setup + query in-process against pgmicro via its Rust API, capturing
   rows/column metadata/error (mapped through the existing
   `limbo_error_to_pg` SQLSTATE mapping).
2. Runs the identical SQL against real Postgres via a `psql` subprocess
   (unaligned/CSV output mode for reliable parsing), capturing the same
   shape of result.
3. Canonicalizes both outputs per §2 (converting the in-process side through
   `encode_value`'s formatting rules so both are comparable as text), diffs,
   reports pass/fail with both raw outputs shown on failure (never just
   "different" — per this project's Fail Loud principle, a silent or vague
   mismatch report is itself a defect).
4. **Exits non-zero on any mismatch** (deliberately unlike `scripts/diff.sh`,
   since this is meant to gate CI, not just assist a human eyeballing
   output).

The Tier 2 (wire) runner follows the same steps but drives `psql` against
both sides (real Postgres, and pgmicro's `--server` mode), skipping the
in-process/encode_value canonicalization step since both sides are already
psql-formatted text.

### 4. Phased rollout

**Phase 1 — harness + re-verify existing coverage.** Build the runner
against a small hand-picked slice of the ~240 existing hardcoded tests
(prioritize `interval.rs`, `type_aliases.rs`, `dialect.rs`'s trickier cases
— anywhere a text-formatting literal was "checked once"). Re-verify each
literal against live `psql` while porting it in. Any literal that turns out
wrong is a **found bug**, reported as such, not quietly corrected without
comment (Fail Loud). Do not attempt to port all ~240 at once — this phase's
goal is proving the harness and canonicalization rules work, using enough
real coverage to be worth the investment, not exhaustive migration.

**Phase 2 — new-feature corpus, aligned to the two-plan candidates.** As
each of the 8 Turso-core plans (sequences, cascading drop, join-filtered
DELETE, unpopulated matview, timestamptz, advisory locks, int4 overflow,
identifier case) and its pgmicro-side follow-up lands, add a corpus fixture
set targeting exactly the trap conditions each plan identified — e.g. for
sequences: `nextval` survives rollback; for unpopulated matview: querying
before `REFRESH` errors, doesn't return empty; for identifier case: quoted
vs. unquoted collision rules. These are the highest-value fixtures because
they're the traps a "reasonable-sounding but wrong" implementation would
pass without ever being checked against real PG.

**Phase 3 — property-based fuzzing.** Once Phase 1/2 prove the
canonicalization rules are solid, adapt `testing/differential-oracle`'s
generation-loop/schema-tracking/reporting *pattern* (not its SQLite
grammar) into a PG-dialect SQL generator, run continuously (nightly CI, not
per-PR — see below), feeding any discovered mismatch back into `corpus/` as
a permanent regression fixture.

### 5. CI integration

Real Postgres in CI needs a `services:` block
(`.github/workflows/pgmicro-ci.yml` currently has none anywhere in the
repo). Recommend a **separate, nightly-scheduled workflow**
(`pgmicro-pg-differential.yml`), not a blocking per-PR job — spinning up a
real Postgres container and running `psql` round-trips is slower and
carries more flakiness risk (container startup, connection races) than
this project's existing fast per-PR checks, and per this codebase's own
principle of not over-building before it's needed, start with visibility
(nightly, reported) before considering blocking-per-PR gating once the
corpus and canonicalization rules have proven stable.

Local dev: document in the new `testing/pg-differential/README.md` that a
reachable Postgres is required (e.g. `docker run -p 5432:5432 postgres`),
mirroring `scripts/diff.sh`'s existing "requires sqlite3 on PATH" pattern —
this is not a new category of environment requirement for this codebase.

**CI gating is a decision for the user, not something to pick unilaterally.**
Recommend starting the new job as allow-failure/informational while the
corpus is thin — a first real run against live Postgres is likely to surface
a batch of genuine, previously-unknown incompatibilities all at once, and
that shouldn't block unrelated PRs on day one. Flip to blocking once the
initial batch of findings is triaged into tracked follow-up work rather than
appearing as a surprise wall of new CI failures.

## Open questions (need a decision, not a guess)

- **Target PostgreSQL version.** Nothing in the repo documents one today.
  Recommend the latest stable PostgreSQL major version at implementation
  time, pinned exactly (e.g. `postgres:16` container tag) in both the CI
  service definition and the local-dev docs — do not leave this
  unpinned/floating, since PG's own behavior can differ across major
  versions and this harness only asserts compatibility with the one pinned
  version.

## Explicitly out of scope

- Replacing any existing hardcoded-expectation test — those stay as fast
  in-process regression tests; this plan adds a slower, exact-ground-truth
  layer alongside them, it does not delete the existing ~240 tests.
- Performance/load comparison — this plan is about result correctness only.
- Comparing against multiple PostgreSQL major versions — pick one
  documented target version (whatever `psql`/server version is used in CI)
  and state it explicitly in the README; cross-version behavior
  differences are a real thing in Postgres itself and out of scope here.
- Building a general-purpose SQL fuzzer framework — Phase 3 reuses
  `differential-oracle`'s existing pattern rather than designing a new one.

## Testing (of the harness itself)

- The runner's own canonicalization logic (row-order handling,
  ignore_columns, SQLSTATE extraction) needs unit tests independent of any
  live Postgres connection — feed it captured `psql` output pairs (both
  matching and deliberately mismatched) and assert correct pass/fail.
- A CI smoke test that the nightly workflow can actually reach a real
  Postgres and get a real version string back, distinct from the
  correctness fixtures themselves — so a broken Postgres container reports
  as an infra failure, not as "all fixtures failed" (which would look like
  a mass regression and cause alarm/triage waste).
