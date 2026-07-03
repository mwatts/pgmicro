# Turso-Core Plan: overflow-checked bounded 32-bit integer type

**Status:** not started. Self-contained Turso-core feature — no PostgreSQL dependency.
This is plan (1) of a two-plan pair (per pgmicro's two-plan-rule convention); the
pgmicro-side follow-up plan (wiring PG's `int4`/`integer` column declarations to
the type this plan adds, and fixing the `int4(x)` cast-function shortcut that
currently bypasses custom typing entirely — see Explicitly out of scope) is not
yet written.

## Problem

Turso's custom-type system already lets a type declare bounded storage via an
`ENCODE` check (see `smallint`, `core/schema.rs:686`:
`CREATE TYPE smallint(value integer) BASE integer ENCODE CASE WHEN value
BETWEEN -32768 AND 32767 THEN value ELSE RAISE(ABORT, 'integer out of range
for smallint') END DECODE value OPERATOR '<'`), but **no 32-bit-range
equivalent exists today**, and critically, **the existing pattern only checks
range at cast/storage time, never at arithmetic-evaluation time** — even for
`smallint` itself.

Verified directly in code, not assumed:

- `smallint`'s `CREATE TYPE` (`core/schema.rs:686`) declares exactly one
  operator, `OPERATOR '<'` — no `+`, `-`, `*`, or `/`. Compare to `numeric`
  (`core/schema.rs:693`), which *does* wire arithmetic:
  `OPERATOR '+' numeric_add OPERATOR '-' numeric_sub OPERATOR '*' numeric_mul
  OPERATOR '/' numeric_div ...`. So even where a bounded custom type exists
  today, arithmetic between two values of that type is **not** dispatched to
  any checked function — it falls through to plain, unchecked base-type
  arithmetic.
- `ENCODE` only executes in two places: at an explicit `CAST` expression
  (`core/translate/expr.rs:8034`, `emit_custom_type_encode_value`) and when a
  value is stored into a column of that type via INSERT/UPDATE
  (`core/translate/insert.rs:4275`, `emit_custom_type_encode` →
  `emit_custom_type_encode_columns`). A bare `SELECT a + b` that never casts
  or stores its result never invokes `ENCODE` at all.
- `int4`/`integer` itself has **no custom type today**. It maps straight to
  the raw base type: `parser_pg/src/translator.rs:4853`,
  `"INTEGER" | "INT" | "INT4" | "SERIAL" | ... => "INTEGER".into()` — no
  wrapper, no range check, indistinguishable from an untyped column. (`bigint`
  is one step further along: it has a custom-type wrapper,
  `core/schema.rs:687`, `CREATE TYPE bigint(value integer) BASE integer`, but
  like `smallint` it declares zero arithmetic operators, so it has the same
  arithmetic-overflow gap this plan is fixing for `int4` — flagged as
  analogous future work below, not this plan's job.)

Net effect, reproducible today: `SELECT CAST(2147483647 AS INTEGER) +
CAST(1 AS INTEGER)` returns `2147483648` silently. Nothing in Turso's engine
distinguishes "this integer is declared to fit in 32 bits" from "this is an
ordinary integer" once you're inside an arithmetic expression, because
nothing currently *asks* — there is no bounded-32-bit type to check against,
and even the one bounded type that does exist (`smallint`) doesn't hook
arithmetic. A generic database engine has every reason to offer a bounded
integer type whose arithmetic operators fail loud on overflow rather than
silently wrapping into a wider range — that's the feature, independent of any
specific SQL dialect that might consume it.

## Design

### The mechanism already exists — this is additive, not a new engine capability

Turso's custom-type system dispatches binary operators through a
**translate-time, statically-resolved** lookup, not a VDBE-opcode-level
runtime branch:

- `core/translate/expr.rs:8228`, `find_custom_type_operator(e1, e2, op, ...)`
  — called once per binary expression during query compilation
  (`core/translate/expr.rs:1339`, inside the general binary-expr translation
  path that already runs for *every* binary expression in *every* query,
  regardless of dialect).
- It resolves "does either operand have a custom type" via
  `expr_custom_type_info_extended` (`expr.rs:8160`), which recognizes two
  operand shapes: a column declared with that custom type
  (`expr_custom_type_info`, `expr.rs:8116`, via `Schema::get_type_def` keyed
  on the column's declared type name) or an explicit `CAST(... AS
  <type>)`/`::type` (`expr_cast_custom_type_info`, `expr.rs:8144`, via
  `Schema::get_type_def_unchecked` keyed on the cast's target type name).
- If a match is found, and the type def has a matching `OPERATOR` entry
  (`TypeDef::operators()`, `core/schema.rs:350`, backed by
  `ast::TypeOperator { op: String, func_name: Option<String> }`,
  `parser/src/ast.rs:1375`), the expression is rewritten to call that
  function instead of the base arithmetic op (`emit_custom_type_operator`,
  `expr.rs:7870`).

This is exactly the mechanism `numeric`, `money`, and `interval` already use
for checked/custom arithmetic (`core/schema.rs:693-695`; e.g.
`core/functions/money.rs:41`, `exec_money_pl`, registered as
`ScalarFunc::MoneyPl` in `core/function.rs:905`/`1712`). Adding a bounded
32-bit type with checked arithmetic requires **no new `Value` variant, no new
VDBE opcode, and no change to `core/vdbe/execute.rs`** — it requires (a) a new
`CREATE TYPE` definition and (b) new scalar functions in the same shape as
`exec_money_pl`, doing `i32::checked_add`/`checked_sub`/`checked_mul` instead
of decimal-money arithmetic.

**Why this is inherently scoped and cannot regress plain integer arithmetic**
(the directive's design point 2): `find_custom_type_operator` only fires when
an operand is a column *declared* with the new type or *explicitly cast* to
it. Plain `int8`/untyped-integer arithmetic has no operand that resolves to
this type def, so the lookup returns `None` and falls through to the existing
base-type path unchanged — exactly as it does today for every query that
doesn't touch `numeric`/`money`/`interval` columns. There is also no added
*cost* to unrelated arithmetic: the lookup already runs once per binary
expression at compile time for every query today (it's how `numeric`/`money`
already work); registering one more custom type just means it can now match
where it previously couldn't, for compile-time-resolvable int4 operands only
— it is not a new per-row check in the VDBE hot loop.

### Two distinct rules — arithmetic-time and cast/storage-time

**Acceptance bar:** this codebase's purpose is reproducing real database
behavior faithfully; PostgreSQL's `int4` is the concrete, verifiable
specification this plan transcribes (even though this type's own
justification, above, stands on its own as a generic bounded-integer
feature). Every rule below must be spot-checked against a live `psql` before
this is considered done — do not trust this document's transcription over an
actual server's output if they disagree.

1. **Arithmetic-evaluation-time overflow** (`+`, `-`, `*`, `/`): real PG
   errors immediately, independent of storage —
   `SELECT 2147483647::int4 + 1;` → `ERROR: integer out of range`, with no
   table, no CHECK constraint, no INSERT involved. This is the rule the
   existing `smallint`/CHECK-only pattern cannot express at all (see
   Problem) — it requires the `OPERATOR` wiring described above:
   ```sql
   CREATE TYPE int4(value integer) BASE integer
     ENCODE CASE WHEN value BETWEEN -2147483648 AND 2147483647
            THEN value ELSE RAISE(ABORT, 'integer out of range') END
     DECODE value
     OPERATOR '<' int4_lt
     OPERATOR '=' int4_eq
     OPERATOR '+' int4_add
     OPERATOR '-' int4_sub
     OPERATOR '*' int4_mul
     OPERATOR '/' int4_div
   ```
   `int4_add`/`int4_sub`/`int4_mul` use `i32::checked_{add,sub,mul}`, erroring
   on `None`. `int4_div` additionally must special-case `i32::MIN / -1`
   (overflows `i32` even though neither operand looks out of range) — real
   PG raises `ERROR: integer out of range` for `SELECT (-2147483648)::int4 /
   -1;`, not a division-by-zero error; verify this exact case against `psql`,
   it is a documented PG edge case (`int4div` in PG's own source checks for
   it explicitly) and easy to miss if only testing "normal" overflow.
2. **Cast/assignment-time overflow**: `SELECT 9999999999::int4;` →
   `ERROR: integer out of range` — a *different* code path from (1), fired by
   `ENCODE` at `CAST`/INSERT/UPDATE, not by an operator. This is exactly what
   `smallint`'s existing `ENCODE` clause already does for its own range
   (`core/schema.rs:686`); the `int4` type above needs only the same
   `ENCODE` shape with the 32-bit bounds, which is why this half of the
   feature is comparatively cheap — the mechanism for it already exists and
   works, it's just never been instantiated for a 32-bit range.

**Open question — unary negation is not covered by this design as written,
flag for the human:** real PG also errors on unary negation overflow:
`SELECT -(-2147483648::int4);` → `ERROR: integer out of range` (negating
`i32::MIN` overflows). Verified in code: unary operators
(`ast::Expr::Unary`) are translated by an entirely separate path
(`core/translate/expr.rs:3920`, matching on `UnaryOperator::Negative` etc.)
that never calls `find_custom_type_operator` — the custom-type `OPERATOR`
mechanism as it exists today only hooks *binary* expressions. Making unary
negation overflow-checked for `int4` therefore needs either (a) a small
parser/typesystem extension to let a `CREATE TYPE ... OPERATOR` entry apply
to a unary operator too (a real, if narrow, `core/`/`parser/` change beyond
what's described above), or (b) accepting this as a known, documented gap for
a first version. This plan does not resolve that choice — it needs a human
call before implementation starts, since it changes the size of the
`parser`/`core/translate/expr.rs` change from "zero" to "small."

### Files touched

- `core/schema.rs` — new `int4` entry in `bootstrap_builtin_types`'s
  `type_sqls` list (alongside `smallint`/`bigint`/`numeric`), per the SQL
  above.
- `core/functions/int4.rs` (new, mirroring `core/functions/money.rs`'s
  shape) — `exec_int4_add`/`exec_int4_sub`/`exec_int4_mul`/`exec_int4_div`/
  `exec_int4_lt`/`exec_int4_eq`, each taking `i32`-range-checked `Value`
  operands and returning `Result<Value, LimboError>`, erroring with a message
  matching PG's `integer out of range` text on overflow.
- `core/function.rs` — register `ScalarFunc::Int4Add`/`Int4Sub`/`Int4Mul`/
  `Int4Div`/`Int4Lt`/`Int4Eq` the same way `MoneyPl` etc. are registered
  (name-to-enum table plus enum-to-name table, `core/function.rs:884-905`
  and `:1691-1712`).
- Tests: unit tests for each scalar function (overflow and non-overflow
  cases), plus an integration test exercising the full `CREATE TYPE` +
  `CAST`/column-typed arithmetic path end to end, **each cross-checked
  against real `psql` output**, per the Acceptance bar above.

## Explicitly out of scope

- **The SQL-level `int4`/`integer` type *name* mapping**: already exists,
  independent of this plan. `parser_pg/src/translator.rs:4853` already maps
  PG's `INTEGER`/`INT`/`INT4`/`SERIAL`-family column types to a Turso type
  name string (today, the raw base type `"INTEGER"`, with no wrapper); and
  `core/pg_catalog.rs:2444-2446` already has a full `pg_type` catalog row for
  `int4` (OID 23) for wire-protocol/introspection purposes, unrelated to
  whether a Turso custom type backs it. This plan's job is only to give the
  engine a bounded, overflow-checked 32-bit integer *representation* to map
  onto — it does not invent new PG type-name parsing, and does not touch
  `pg_catalog.rs`. Repointing `translator.rs:4853`'s target string from
  `"INTEGER"` to the new `"int4"` type name is pgmicro-side wiring, left for
  the follow-up plan.
- **The `int4(x)`/`int2(x)`/`int8(x)` function-style cast shortcut**
  (`parser_pg/src/translator.rs:3473-3475`): today these translate directly
  to `CAST(x AS INTEGER)`, bypassing even the existing `smallint` custom type
  entirely (so `int2(40000)` doesn't range-check today, a pre-existing,
  separate gap). Fixing this to route through the new `int4` type (or the
  existing `smallint`) is pgmicro/translator-side wiring, not a core change,
  and is left for the follow-up plan.
- **`smallint`/`int2` and `bigint`/`int8` gaining the same arithmetic-operator
  treatment**: both have the identical missing-`OPERATOR` gap documented in
  Problem above (zero arithmetic operators declared today). Real PG's
  `smallint`/`bigint` have the same overflow-on-arithmetic behavior as
  `int4`. Out of scope for this plan (which is scoped to the specific 32-bit
  request), but the exact same design — new checked scalar functions plus
  `OPERATOR` wiring on the existing type defs — applies directly and should
  be flagged as natural, low-effort follow-on work once this plan's pattern
  lands.
- **`numeric`/`decimal`**: already has checked arithmetic operators
  (`core/schema.rs:693`) — not this plan's concern.
- **Unary negation overflow**: see Open question above — explicitly deferred
  pending a human decision, not solved by the design as written.
- **Performance tuning**: `i32::checked_*` is a single hardware-supported
  branch; no perf concern.

## Testing

- `cargo test -p turso_core` for the new `int4` scalar-function unit tests
  and the `CREATE TYPE`/schema bootstrap test.
- `cargo fmt` / `cargo clippy --workspace --all-features --all-targets --
  --deny=warnings`.
- Manual check via `cargo run -q --bin tursodb -- -q`, **and the same
  statements run against a real `psql` to confirm the expected result below
  before trusting it**:
  ```sql
  SELECT CAST(2147483647 AS int4) + CAST(1 AS int4);        -- ERROR: integer out of range
  SELECT CAST(-2147483648 AS int4) - CAST(1 AS int4);       -- ERROR: integer out of range
  SELECT CAST(2000000000 AS int4) * CAST(2 AS int4);        -- ERROR: integer out of range
  SELECT CAST(-2147483648 AS int4) / CAST(-1 AS int4);      -- ERROR: integer out of range (not div-by-zero)
  SELECT CAST(100 AS int4) + CAST(1 AS int4);               -- 101 (no error, ordinary case)
  SELECT CAST(9999999999 AS int4);                          -- ERROR: integer out of range (cast-time, not arithmetic)
  SELECT CAST(100 AS int4) < CAST(200 AS int4);             -- 1 (comparison unaffected)
  ```

## Migration / compatibility

Pure addition: a new custom type name (`int4`) that nothing references until
the pgmicro follow-up plan maps PG's `integer`/`int4` column type onto it.
No existing type's behavior changes — `smallint`/`bigint`/`numeric`/etc. and
plain untyped integer arithmetic are untouched, since `find_custom_type_operator`
only matches operands whose declared/cast type resolves to the new type def.
No schema/storage format change, no new public API surface beyond the new
type name and its scalar functions.
