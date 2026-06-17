# Design: INTERVAL and MONEY Type Fidelity

**Status:** Implemented on `pgmicro-fixes`  
**Land commits:** `c97de3dd5` (core types), `2b084eed8` (translator, catalog, wire)  
**Prior state:** pgmicro mapped `INTERVAL → TEXT` and `MONEY → REAL`, breaking interval
arithmetic, timestamp math, and money precision.

## Summary

Turso-core custom types with PostgreSQL-compatible semantics, wired through pgmicro
translation, catalog, and wire encoding.

**Scope includes full calendar interval semantics** (`'1 month' + '1 month' ≠ '60 days'`),
**`justify_hours` / `justify_days`**, and the **`extract(... FROM interval)` suite**.

## Goals

| Area | Requirement |
|------|-------------|
| Storage | PG 96-bit interval `{months: i32, days: i32, microseconds: i64}`; money as int64 cents |
| Calendar | Month/year fields use calendar math on timestamps; interval `+` is field-wise |
| Literals | `INTERVAL '1 day'`, `'2 months 3 days'`, ISO 8601 `P1DT2H` |
| Timestamp math | `timestamp ± interval`, `timestamptz ± interval` with calendar months |
| Interval ops | `+`, `-`, `*`, `/`, unary `-` |
| Justify | `justify_days`, `justify_hours` match PostgreSQL |
| Extract | `epoch`, `year`, `month`, `day`, `hour`, `minute`, `second`, `milliseconds`, `microseconds` from interval |
| Money | Fixed-scale cents; `+`, `-`, `*`, `/`; `$` formatting |
| Errors | Invalid input → constraint/overflow errors with SQLSTATE 22007/22003 |
| Tests | Integration tests in `tests/integration/postgres/interval.rs`; core unit tests in `core/interval/` |

## Non-Goals (deferred)

- Locale-aware money (`lc_monetary`)
- Binary wire encoding for interval/money parameters
- `NUMERIC` wire precision (separate EVALUATION item)

## Architecture

```
PG SQL → libpg_query → translator (literals, casts, timestamp±interval, EXTRACT)
                              ↓
                    Turso AST + custom type operators
                              ↓
              core/interval + core/money (parse, arithmetic, justify, extract)
                              ↓
                    VDBE scalar funcs → BLOB/INTEGER storage
```

## Turso Core: INTERVAL

### Module layout

```
core/interval/
  mod.rs       — Interval struct, blob I/O, parse/format
  arithmetic.rs — field-wise +/-, scale */ 
  justify.rs   — justify_days, justify_hours
  extract.rs   — extract field helpers + to_epoch_seconds
  timestamp.rs — calendar timestamp ± interval (chrono)
```

### Storage (16-byte LE blob)

| Offset | Type | Field |
|--------|------|-------|
| 0 | i32 LE | months |
| 4 | i32 LE | days |
| 8 | i64 LE | microseconds |

### Calendar semantics

**Interval + Interval:** field-wise add (months, days, microseconds independently).
No normalization unless `justify_*` is called.

```sql
-- PostgreSQL behavior we match:
'1 month'::interval + '1 month'::interval  → '2 mons'     (NOT '60 days')
'30 days'::interval + '30 days'::interval  → '60 days'
```

**Timestamp + Interval:** apply in order:
1. Add `months` with calendar month arithmetic (`checked_add_months`)
2. Add `days` with calendar day arithmetic
3. Add `microseconds` to time component

Uses `chrono` on parsed `timestamp`/`timestamptz` text values (same path as existing
`timestamp` custom type).

### justify_days / justify_hours

Match PostgreSQL `justify_days` / `justify_hours`:

- **justify_days:** `days += months * 30; months = 0`
- **justify_hours:** promote whole days into the time field as hours
  (`microseconds += days * 86400 * 1e6; days = 0`), then normalize microsecond
  overflow into days (carry/borrow)

### extract(... FROM interval)

| Field | Semantics |
|-------|-----------|
| `epoch` | `months * (365.25/12) * 86400 + days * 86400 + microseconds/1e6` (float8) |
| `year` | `months / 12` (trunc toward zero) |
| `month` | `months % 12` (positive remainder) |
| `day` | `days` |
| `hour` | `(microseconds / 3_600_000_000) % 24` after justify_hours optional |
| `minute` | derived from microseconds |
| `second` | derived from microseconds |
| `millisecond` | derived from microseconds |
| `microsecond` | `microseconds % 1_000_000` |

Scalar helpers: `interval_extract(field, blob)` used by translator for `EXTRACT`.

### Builtin custom type

```sql
CREATE TYPE interval(value text) BASE blob
  ENCODE interval_in(value)
  DECODE interval_out(value)
  OPERATOR '+' interval_pl
  OPERATOR '-' interval_mi
  OPERATOR '*' interval_mul
  OPERATOR '/' interval_div
  OPERATOR '<' interval_lt
  OPERATOR '=' interval_eq
```

Registered in `bootstrap_builtin_types()` alongside `numeric`, `timestamp`, etc.

### Scalar functions

| Name | Args | Purpose |
|------|------|---------|
| `interval_in` | text | Parse → blob |
| `interval_out` | blob | Format → PG text |
| `interval_pl` / `interval_mi` | blob, blob | Field-wise +/- |
| `interval_mul` / `interval_div` | blob, float | Scale |
| `justify_days` | blob | Normalize months→days |
| `justify_hours` | blob | Normalize days→time |
| `interval_extract` | text field, blob | EXTRACT support |
| `timestamp_pl_interval` | text, blob | timestamp + interval |
| `timestamp_mi_interval` | text, blob | timestamp - interval |

## Turso Core: MONEY

int64 cents (PostgreSQL internal representation).

```sql
CREATE TYPE money(value text) BASE integer
  ENCODE money_in(value) DECODE money_out(value)
  OPERATOR '+' money_pl OPERATOR '-' money_mi
  OPERATOR '*' money_mul OPERATOR '/' money_div
  OPERATOR '<' money_lt OPERATOR '=' money_eq
```

Negative values formatted as `($1.23)` in v1 (US locale).

## pgmicro Integration

### translator.rs

1. Map `INTERVAL → interval`, `MONEY → money` in `map_pg_type_to_turso` and casts
2. Translate `INTERVAL '...'` literals (pg_query `IntervalConst` node)
3. Rewrite `timestamp_expr ± interval_expr` → `timestamp_pl_interval` / `timestamp_mi_interval`
4. Map `EXTRACT(field FROM interval_expr)` → `interval_extract('field', expr)`
5. Map `justify_days(h)` / `justify_hours(h)` func calls

### pg_catalog + wire

- OIDs: interval=1186, money=790
- `format_type`, `pg_input_error_info`
- `sqlite_type_to_pg_type("interval")` → `Type::INTERVAL`

## Testing

| Layer | Location | Coverage |
|-------|----------|----------|
| Unit | `core/interval/*` tests | parse round-trip, calendar +/-, justify, extract, epoch |
| sqltest | `testing/sqltests/tests/types/interval.sqltest` | storage, operators |
| Integration | `tests/integration/postgres/dialect.rs` | `NOW() - INTERVAL '1 day'`, month arithmetic |
| Integration | `tests/integration/postgres/interval.rs` (new) | justify_*, extract suite |
| Parser | `parser_pg/tests/` | literal translation AST |

Reference vectors from PostgreSQL 16 where possible.

## Key Decisions

1. **Two stacks** (Turso core, then pgmicro) — two-plan rule, minimize merge conflicts
2. **Full calendar month semantics in v1** — no deferred gap; chrono `Months` for timestamp math
3. **Field-wise interval addition** — `'1 mon' + '1 mon' = '2 mons'`; epoch extract uses 365.25/12
4. **justify_* as scalar funcs** — callable from SQL and translator
5. **extract via `interval_extract` helper** — keeps EXTRACT translation thin
6. **Money as int64 cents** — not numeric wrapper; matches PG internals
7. **16-byte LE blob** — stable, documented, independent of PG server binary format

## PR Plan

### Stack A: Turso Core

| PR | Title | Depends |
|----|-------|---------|
| A1 | `feat(interval): core type — parse, format, arithmetic, justify, extract` | — |
| A2 | `feat(interval): register builtin custom type + scalar funcs` | A1 |
| A3 | `feat(interval): timestamp ± interval with calendar semantics` | A1 |
| A4 | `feat(money): builtin custom type + scalar funcs` | — |
| A5 | `test: sqltest interval and money types` | A2, A4 |

### Stack B: pgmicro

| PR | Title | Depends |
|----|-------|---------|
| B1 | `fix(pg): map INTERVAL/MONEY to custom types` | A2, A4 |
| B2 | `fix(pg): translate INTERVAL literals, timestamp±interval, EXTRACT, justify_*` | B1, A3 |
| B3 | `fix(pg): catalog and wire encoding for interval/money` | B1 |
| B4 | `test(pg): integration tests for interval/money` | B2, B3 |
| B5 | `docs: EVALUATION.md — interval/money fixed` | B4 | ✅ done |

## Open Questions

1. **Upstream merge order:** Land A1–A5 on Turso main before B stack, or carry on `pgmicro-fixes`?
   - *Working assumption:* feature branches off `pgmicro-fixes`; upstream PRs filed in parallel.

2. **Interval range bounds:** Reject intervals exceeding PG limits (~±178000000 years)?
   - *Recommendation:* Yes; raise overflow on encode/arithmetic.