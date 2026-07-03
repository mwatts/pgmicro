# Turso-Core Plan: JSON/JSONB-aware containment for `array_contains_all`

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
follow-up pgmicro-side plan is `2026-07-03-pgmicro-json-containment-followup.md`.

## Problem

`array_contains_all(a, b)` (`core/vdbe/array.rs:539-553`) is documented and named as an
array function, but it is reachable from SQL on any input value, including values
produced by `json()`/`jsonb()`. Today, calling it with a JSON document instead of an
array does not error — it silently returns `NULL`:

```
SELECT array_contains_all(json('{"a":1,"b":2}'), json('{"a":1}'));
-- returns NULL today (should return 1 -- b's keys/values are a subset of a's)
```

**Root cause** (verified by reading the code): `array_values_from_any()`
(`core/vdbe/array.rs:23-30`) only knows how to extract elements from two shapes:

- `Value::Blob` → parsed as a SQLite record-format array blob
  (`array_values_from_blob`, backed by `make_array_from_registers`'s
  `ImmutableRecord` encoding)
- `Value::Text` → parsed as a PG-style `{...}` text array literal
  (`parse_text_array`)

Neither branch recognizes a JSONB binary blob (produced by `jsonb()`, distinct
binary format from the record-format array blob — see `core/json/jsonb.rs`) or a
JSON text string with `TextSubtype::Json` (produced by `json()`, `core/types.rs:62-92`).
For those inputs, `array_values_from_any` returns `None`, and both
`exec_array_contains_all`/`exec_array_overlap` fall through to `Value::Null`
(`core/vdbe/array.rs:524-529`, `543-548`) — a silent wrong-result, not an error.

This is surprising for any Turso user storing JSON documents and checking
containment (a common query shape: "does this JSON column's document contain this
key/value?"), independent of any PostgreSQL use case.

## Design

Extend `array_contains_all` to detect JSON/JSONB input and, when detected, perform
structural containment instead of array-element-set containment. Do **not** extend
`array_overlap` — containment (`a ⊇ b`) is a well-defined structural relationship for
JSON objects and arrays; a generic "overlap" relationship is not (there is no
standard meaning for "these two JSON documents overlap"), so `array_overlap` keeps
its current array-only behavior and continues returning `NULL` for JSON input. Adding
JSON semantics only where a well-defined semantics exists avoids inventing a
public API surface nobody asked for.

### Detecting JSON vs. array input

Both PG arrays and JSON/JSONB share Turso's `Value::Blob`/`Value::Text` shape — there
is no distinct `Value::Array` or `Value::Json` variant (`core/types.rs:261-266`).
`Value::Text` carries a `TextSubtype` (`core/types.rs:62-92`): `Text::json(..)`
tags text as JSON (used by `json_type` et al., `core/json/mod.rs:659`). That
resolves the `Value::Text` case for free: check the subtype before falling back to
`parse_text_array`.

`Value::Blob` has no subtype field, so a JSONB blob and a record-format array blob
are indistinguishable by type alone — only by content. The existing `json_type`/
`json_valid`/etc. path already solves this exact problem via
`convert_dbtype_to_jsonb(value, Conv::Strict)` (`core/json/mod.rs:656`), which
performs strict JSONB header/structure validation and errors on malformed input.
Reuse that as the discriminator: attempt strict JSONB parse first; only on failure,
fall back to the existing record-blob array parse.

**Requires verification, not assumption** (per this project's "validate your
hypotheses" rule): confirm strict JSONB parsing actually rejects a well-formed
`ImmutableRecord` array blob rather than false-accepting garbage from
misinterpreted header bytes. Add a unit test that round-trips
`make_array_from_registers(...)` output through `Jsonb::from_raw_data` +
strict-mode validation and asserts it errors, before relying on try-JSONB-first
ordering. If it does not reliably reject, order must instead be array-parse-first
with JSON attempted only on array-parse failure, and that ordering needs the same
false-positive check run in the other direction.

### Containment semantics

**Acceptance bar:** this codebase's actual purpose is reproducing PostgreSQL
behavior on top of Turso, so "reasonable-sounding generic containment design" is
not the bar — bit-for-bit match with real `jsonb @>`/`<@` is. The rules below are
transcribed from PostgreSQL's actual (non-obvious, documented-with-exceptions)
`JsonbDeepContains` behavior, not invented generically. Every rule and every test
case in this section must be spot-checked against a live `psql` instance before
this is considered done — do not trust this document's transcription over an
actual PG server's output if they ever disagree.

Two distinct rules apply, and it is critical they are not conflated: a **top-level-only**
special case, and a **general recursive rule** used everywhere else (including at
every nested level *inside* the general rule — the special case does not recurse):

- **Top-level special case:** if the outer call's `b` (right-hand operand) is a
  bare scalar and `a` (left-hand operand) is an array, the result is simply
  "does `a` have an element equal to `b`" (linear scan, scalar equality). This
  is a real, documented PG quirk:
  `'[1, 2, 3]'::jsonb @> '3'::jsonb` → `true`.
  **This exception applies only once, at the outermost call — never during
  recursion.** Verified against PG's own documented counter-example:
  `'[1, 2, [1, 3]]'::jsonb @> '[1, 3]'::jsonb` → `false`, even though `3`
  "appears inside" the nested array `[1,3]` — because at the point that nested
  array is checked, the comparison is array-element-vs-array-element (general
  rule), not a fresh top-level scalar-vs-array call, so the exception does not
  fire and `[1,3]` does not "contain" bare `3` in that recursive context. A
  correct implementation therefore needs two functions: a public entry point
  that checks for this case once, and a private recursive worker that never
  re-applies it — not a single self-recursive function, or the exception will
  leak into recursion and silently diverge from PG.
- **General recursive rule** (used for everything else, including every
  recursive step):
  - **object ⊇ object:** every key in `b` exists in `a`; for each shared key,
    `a[key]` recursively satisfies this same rule against `b[key]` (scalars:
    equality; nested containers: recurse). Extra keys in `a` not in `b` are
    fine.
  - **array ⊇ array:** every element of `b` matches *some* element of `a`,
    where "matches" is this same recursive rule applied to that pair of
    elements (order-independent, not positional; duplicates in `b` are fine
    even if `a` has only one copy).
  - **scalar ⊇ scalar:** equality — but equality must be *value* equality, not
    encoding equality: `1` and `1.0` are the documented-equal JSON numbers in
    PG's jsonb comparison (numeric value compare, not textual/byte compare),
    and JSON `null` equals JSON `null` (this is the in-document JSON literal
    null, an `ElementType::NULL` scalar — distinct from the SQL `NULL`
    short-circuit below, which is about the *function's own arguments* being
    absent, not a JSON `null` value inside a document).
  - **mismatched shapes at this level** (object vs. array, array vs. scalar,
    object vs. scalar) — anywhere except the one top-level exception above —
    `false`, not an error.
- **either function argument is SQL `NULL`:** `NULL` (matches existing
  `exec_array_contains_all` NULL-propagation, `core/vdbe/array.rs:540-542`).
  Do not confuse this with a JSON `null` *value* inside a document, which
  participates in ordinary scalar equality per the rule above.

Implement via the existing traversal primitives in `core/json/jsonb.rs` rather than
re-parsing to a separate tree type: `Jsonb::element_type()` (`jsonb.rs:961`) to
branch on shape, `Jsonb::object_iterator()`/`array_iterator()`
(`jsonb.rs:3115,3153`) plus `container_property_iterator()` (`jsonb.rs:3205`) to
walk entries/elements without allocating an intermediate `serde_json::Value` tree.

### Files touched

- `core/vdbe/array.rs` — `exec_array_contains_all`: detect JSON/JSONB (per above),
  dispatch to a new `json_contains(a: &Jsonb, b: &Jsonb) -> bool` public entry
  point when either side is JSON. `json_contains` checks the top-level
  scalar-in-array exception once, then delegates to a separate, private
  `json_deep_contains(a: &Jsonb, b: &Jsonb) -> bool` for the general recursive
  rule — keep these as two distinct functions (see **Containment semantics**);
  do not implement this as one function that calls itself, or the top-level
  exception will leak into recursive calls and silently diverge from real PG.
  Keep the current array-element-set logic as the fallback for genuine
  (non-JSON) arrays.
- `core/json/jsonb.rs` or a new `core/json/containment.rs` (implementer's call,
  guided by "one clear responsibility per file" — if the recursive containment
  logic plus its tests exceed ~150 lines, a dedicated file is cleaner than growing
  the already-large `jsonb.rs`) — the `json_contains`/`json_deep_contains` pair.
- Tests: unit tests alongside the implementation, **each cross-checked against
  real `psql` output, not derived from this document alone**: object subset,
  array subset, nested containment, the top-level scalar-in-array exception
  (`'[1,2,3]' @> '3'` → true) and its documented non-recursion counter-example
  (`'[1,2,[1,3]]' @> '[1,3]'` → false), numeric value-equality across
  representations (`1` vs `1.0`), JSON-`null`-equals-JSON-`null`, mismatched
  shapes → false, SQL-`NULL`-argument propagation, and the JSONB-vs-array-blob
  discriminator false-positive check above — plus a `core/vdbe/array.rs`
  integration-level test exercising `SELECT array_contains_all(jsonb(...), jsonb(...))`
  end to end.

## Explicitly out of scope

- **`array_overlap`:** not extended (no well-defined JSON "overlap" semantics —
  see Design). If a caller passes JSON to it, current NULL-return behavior is
  unchanged.
- **New SQL operators** (e.g. a `@>`-style infix operator in Turso's own SQL
  grammar): not proposed here. This plan only changes existing function behavior;
  no grammar/parser changes. Any dialect that wants infix operator sugar over this
  function is a separate, dialect-specific concern (see the pgmicro follow-up
  plan for exactly that).
- **`json_type`/`json_valid`-family changes:** none needed; this plan only reuses
  their existing strict-parse validator as a discriminator, read-only.
- **Performance tuning for large documents:** the recursive walk is O(size of b ×
  size of a) in the worst case (naive object/array element matching, no
  pre-indexing). Acceptable for a first correct implementation; flag as a
  follow-up if a real workload needs it — do not speculatively optimize now.

## Testing

- `cargo test -p turso_core json` (or the crate's actual json test module path)
  for the new containment unit tests.
- `cargo test -p turso_core array` for the `core/vdbe/array.rs` integration test.
- `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` and
  `cargo fmt` per standard workflow.
- Manual check via `cargo run -q --bin tursodb -- -q`, **and the same statements
  run against a real `psql` to confirm the expected column below before trusting
  it**:
  ```sql
  SELECT array_contains_all(jsonb('{"a":1,"b":2}'), jsonb('{"a":1}'));       -- 1
  SELECT array_contains_all(jsonb('{"a":1}'), jsonb('{"a":2}'));             -- 0
  SELECT array_contains_all(jsonb('[1,2,3]'), jsonb('[2,3]'));               -- 1
  SELECT array_contains_all(jsonb('{"a":1}'), jsonb('[1]'));                 -- 0 (mismatched shape)
  SELECT array_contains_all(jsonb('{"a":1}'), NULL);                        -- NULL
  SELECT array_contains_all(jsonb('[1,2,3]'), jsonb('3'));                  -- 1 (top-level scalar-in-array exception)
  SELECT array_contains_all(jsonb('[1,2,[1,3]]'), jsonb('[1,3]'));          -- 0 (exception does NOT recurse)
  SELECT array_contains_all(jsonb('[1,2,[1,3]]'), jsonb('[[1,3]]'));        -- 1 (ordinary recursive array match)
  SELECT array_contains_all(jsonb('{"a":1}'), jsonb('{"a":1.0}'));          -- 1 (numeric value equality, not encoding equality)
  SELECT array_contains_all(jsonb('{"a":null}'), jsonb('{"a":null}'));      -- 1 (JSON null equals JSON null)
  ```

## Migration / compatibility

Pure behavior extension: previously-NULL results for JSON input become real
`0`/`1` results. No existing non-NULL result changes for any input (array
inputs are untouched; the new JSON branch is only reachable for inputs the old
code already treated as unusable). No schema/storage format change, no new
public API, no new dependency.
