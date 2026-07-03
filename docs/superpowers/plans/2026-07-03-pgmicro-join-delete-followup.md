# pgmicro Plan: Translate `DELETE ... USING` (Task B17 follow-up)

**Depends on:** `2026-07-03-turso-core-join-delete.md` landing first (adds
`Stmt::Delete.from`, `DeletePlan.from_tables`/`write_set_plan`, and — as a
required prerequisite — the write-set scratch-table dedup fix shared with
`UPDATE ... FROM`). Do not start the parser_pg translation change before that
core plan merges: the AST field this plan writes into does not exist yet.

## Context

Task B17 in `2026-07-02-pgmicro-fixes.md` shipped a fail-loud rejection for
`DELETE ... USING` rather than silently ignoring the join filter: confirmed
today at `parser_pg/src/translator.rs:1375-1381` (`translate_delete`), which
returns a hard `ParseError` as soon as `delete.using_clause` is non-empty,
with a regression test locking this in
(`parser_pg/src/translator.rs:7474`, `test_delete_using_rejected_not_silently_ignored`).
That was the correct interim move per this project's "reject unsupported
syntax with a clear error over silently wrong results" rule — but it is not
the end state. This plan replaces the rejection with real translation once
the Turso-core primitive exists.

## What this plan actually does

1. **Delete the rejection, translate the clause instead.** `delete.using_clause`
   (the libpg_query protobuf field) is already fully populated by the time
   `translate_delete` runs — libpg_query is PostgreSQL's real parser, so
   `USING t2, t3` arrives pre-parsed as a list of range items; today's code
   only checks `is_empty()` before erroring, it never reads the contents.
   This is the **same protobuf shape** `translate_update` already consumes:
   compare `translate_update`'s handling of `update.from_clause` at
   `parser_pg/src/translator.rs:1349-1351`:
   ```rust
   let from = if !update.from_clause.is_empty() {
       Some(self.translate_from_items(&update.from_clause)?)
   } else {
       None
   };
   ```
   `translate_delete` should do the direct analog once `ast::Stmt::Delete`
   gains a `from` field:
   ```rust
   let from = if !delete.using_clause.is_empty() {
       Some(self.translate_from_items(&delete.using_clause)?)
   } else {
       None
   };
   ```
   reusing the existing `translate_from_items` helper (`translator.rs:2022`)
   verbatim — no new join-translation logic needed on the parser_pg side.
   This makes the pgmicro-side change small: the heavy lifting is entirely
   in the core plan's `DeletePlan`/emitter work.

2. **Remove or repurpose the now-obsolete rejection test.** Once translation
   is real, `test_delete_using_rejected_not_silently_ignored`
   (`translator.rs:7474`) asserts behavior this plan deliberately removes.
   Do not leave it in place asserting the old wrong-on-purpose behavior —
   either delete it or repurpose it into a positive translation test
   (assert `Stmt::Delete.from` is `Some(..)` with the expected joined
   tables), matching how other superseded fail-loud tests in this codebase
   get replaced rather than left dangling.

3. **End-to-end integration tests against the new core behavior**, mirroring
   `tests/integration/postgres/update_from.rs`'s existing structure (which
   currently has no multi-match-target-row or self-referential-join test —
   confirmed via `grep -n "^fn test" tests/integration/postgres/update_from.rs`,
   which lists only `basic`, `multi_column`, `with_expression`, `no_match`,
   `subquery`, `multiple_tables`, `returning`). Add a new
   `tests/integration/postgres/delete_using.rs` covering:
   - Basic single-match `DELETE ... USING` (one target row, one join match).
   - No-match (target row survives, matching `update_from.rs`'s no-match
     test shape).
   - Multi-table `USING t2, t3` (two joined tables in one clause).
   - **Multi-match target row** — the core plan's flagged highest-risk case:
     one target row matching N USING-side rows. Assert exactly one row
     deleted (`DELETE 1`, not `DELETE N`) and exactly one `RETURNING` row.
   - Self-referential `USING` (`DELETE FROM t USING t AS t2 WHERE ...`, the
     "delete duplicates" idiom) — the core plan flags this as needing a real
     `psql` cross-check for which of the duplicate rows actually gets
     deleted before hardcoding an expected id in any test; do that
     verification here, do not assume.
   - `RETURNING` pulling columns from both the target table and a
     USING-table.

   Also add the equivalent multi-match-target-row test to the **existing**
   `update_from.rs` (not just the new file) — the core plan's dedup fix
   changes `UPDATE ... FROM`'s behavior for this case too, and that change
   needs its own regression test independent of the new DELETE feature,
   since nothing today exercises it.

## Testing

Per this project's Core Principle 6 ("test with the REPL first, psql
second"): exercise every case above via `cargo run -p pgmicro -- :memory:`
first. Then:
- `cargo test -p turso_parser_pg` (translator unit tests, including the
  repurposed/removed rejection test).
- `cargo test -p core_tester --test integration_tests integration::postgres`
  (new `delete_using.rs`, amended `update_from.rs`).
- `cargo fmt`, `cargo clippy --workspace --all-features --all-targets --
  --deny=warnings`.
- Every RETURNING/multi-match/self-join semantic claim in the new tests must
  be checked against a live `psql` instance first, per the core plan's own
  testing section — do not hardcode an expected value derived only from
  reading documentation.

## Out of scope

- Everything the Turso-core plan already excludes: MySQL-style multi-target
  `DELETE FROM t1, t2 USING ...`, `ORDER BY`/`LIMIT` combined with `USING`
  (real PostgreSQL doesn't support this combination either), and performance
  tuning of the write-set scratch table.
- Any translator-side join-graph logic beyond calling the existing
  `translate_from_items` helper — if that helper turns out insufficient for
  some USING-clause shape, that is itself a core-side (or shared parser_pg
  helper) concern, not something to special-case in `translate_delete`.
