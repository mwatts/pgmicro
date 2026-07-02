# pgmicro Quality Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix all Critical/High/Medium/Low findings from `PGMICRO_QUALITY_REVIEW.md` — panics, data-loss bugs, silently-dropped SQL clauses, catalog inaccuracies, and protocol-isolation bugs — without expanding scope beyond what each finding requires.

**Architecture:** Six independent workstreams, each owning a disjoint set of files, executable in parallel git worktrees by separate subagents. Two findings (H6, C8/H20) have a cross-workstream interface documented explicitly below; everything else is fully independent. Findings that require new Turso-core capability (sequences, JSONB polymorphic ops, CASCADE, correlated USING-deletes, constraint-name resolution) get an interim fail-loud rejection now and a deferred two-plan follow-up — they are not force-fit into the translator.

**Tech Stack:** Rust, `pg_query` (libpg_query C FFI) for PG parsing, `turso_parser::ast` as the shared AST, Turso VDBE bytecode engine, `pgwire` crate for wire protocol, tokio async runtime, SQLite-compatible storage.

## Global Constraints

- Conventional commits: `type(scope): message` (feat, fix, docs, refactor, test, chore, perf). Sign every commit (`-S`).
- `cargo fmt` and `cargo clippy --workspace --all-features --all-targets -- --deny=warnings` must pass before any commit that touches `.rs` files.
- Release/fuzz profiles use `panic = "abort"` — any reachable panic in `core/` or `parser_pg/` kills the entire server process. Panic-fixing tasks (C1, C2, C3) are the highest-severity work in this plan.
- Two-plan rule (pgmicro CLAUDE.md): features needing Turso-core changes get a Turso-core plan (no mention of postgres) plus a separate pgmicro plan. This plan does **not** write those two-plan documents — it identifies which findings need them and ships an interim reject-with-clear-error fix instead.
- Minimize `core/` changes — every line changed in `core/` is a future merge conflict with upstream Turso.
- Translator correctness over coverage — reject unsupported syntax with a clear error rather than silently producing wrong results.
- Test with the REPL / Rust integration tests first; `psql` wire testing is verification, not the primary test path.
- No silent caps or partial fixes: if a finding is too large for a bite-sized task (H24, H26, O(N²) rescans), this plan says so explicitly rather than fabricating a fake minimal fix.

---

## Workstream Map (parallel execution)

| Workstream | Owns (files) | Findings | Worktree |
|---|---|---|---|
| **A — Wire/Protocol** | `cli/pg_server.rs`, `core/connection.rs` (server-facing bits), `core/numeric/decimal.rs`, `pgmicro/tests/pgmicro.rs` | C4, C5, C6, H21, H22, H23, M1, M2, M3 | `wt-wire` |
| **B — Translator** | `parser_pg/src/translator.rs` | C7, H1–H13, H20, M1–M13 (translator) | `wt-translator` |
| **C — Dialect/Dispatch** | `core/pg_dispatch.rs` | C8, H18, H19 | `wt-dialect` |
| **D — Core Semantics** | `core/vdbe/value.rs`, `core/vdbe/execute.rs` (LIKE/divide paths only), `core/regexp.rs` (comment-only) | H14, H17 | `wt-semantics` |
| **E — Catalog/Functions** | `core/functions/postgres.rs`, `core/pg_catalog.rs`, `core/pg_comment.rs`, `core/pg_role.rs`, `core/function.rs`, `core/vdbe/execute.rs` (call-site `?` only), `tests/integration/postgres/catalog.rs` | C1, C2, C3, H24–H32, catalog mediums/lows | `wt-catalog` |
| **F — REPL/Packaging** | `pgmicro/src/main.rs`, `npm/pgmicro/*` | H33, H34, packaging mediums | `wt-repl` |

### Cross-workstream interfaces (must sequence, not parallelize blindly)

1. **B → C interface (H20 / C8):** Translator's `PgDropSchemaStmt` struct changes `name: String` → `names: Vec<String>` (never empty). Dialect's `handle_pg_drop_schema` in `core/pg_dispatch.rs` must consume `names` instead of `name`. **B's H20 task must land (or at least be code-complete and shared) before C's C8 task**, or C8 should be written against the new shape from the start. Recommended order: B lands H20 first, merges to a shared integration branch, then C starts C8 against the updated struct.
2. **H6 conflict — RESOLVED:** Translator workstream (B) fixes `~*`/`!~*` case-insensitive regex by prepending `(?i)` via `ast::Operator::Concat`, reusing the existing case-sensitive `regexp()` in `core/regexp.rs` — **zero core changes**, verified against `regex` crate's actual inline-flag support. This is the adopted fix (Task B-H6 below). The alternative approach of adding a new `regexp_i()` core function is **rejected**: it touches `core/` unnecessarily (violates "minimize core/ changes") and is redundant given the verified inline-flag behavior. Do not implement `regexp_i`; if it exists in any branch, delete it as cleanup.
3. **`cli/pg_server.rs` — intra/cross-workstream file overlap:** Workstream A owns this file. Task A-C4.3 (per-connection refactor of `TursoPgHandler`) must land before Workstream C's dialect-file-cleanup task that also touches `TursoPgHandler`'s schema-file bookkeeping — **C's file-cleanup subtask is deferred until A-C4 is merged** (noted again in Task C's block). Workstream D's H17 bonus SQLSTATE fix (`classify_constraint_sqlstate`) and Workstream A's H21/H22/H23 edits are in the same function/file but non-overlapping line ranges — land in either order, rebase before merge.
4. **`core/vdbe/execute.rs` — E vs D overlap:** Workstream E's C2 call-site fix (lines ~7205-7266, `?` propagation for lpad/rpad/repeat) and Workstream D's H14/H17 edits (~line 7140 Like arm, ~410-424 op_divide) touch the same file at different line ranges. Land either first; the other rebases. No logical dependency.
5. **Intra-A:** M3 (NOTIFY leak fix) and A-C4.5 (close/cleanup) both touch `cli/pg_server.rs:1626-1647` — implement as one commit within Workstream A, not two competing edits.

---

## Workstream E — Catalog/Functions (worktree `wt-catalog`)

Owns: `core/functions/postgres.rs`, `core/pg_catalog.rs`, `core/pg_comment.rs`, `core/function.rs`, `core/vdbe/execute.rs` (call-site `?` propagation only, lines ~7205-7266), `tests/integration/postgres/catalog.rs`.

Landing order: **E1, E2, E3 (Critical, fully independent, parallelize) → E-dead-tests (0-risk sanity net, land early) → E4 (H25) + E10 (H31) same engineer → E6 (H27) → E7 (H28) + E-low-attalign (bundle) → E8 (H29) → E9 (H30) → E11 (H32) + E-low-trailing-space (bundle) → E-medium tasks as capacity allows → E5 (H24) and E-attached-schema (H26) held back — see Notes.**

### Task E1: Fix gcd()/lcm() integer-overflow panic (C1)

**Files:**
- Modify: `core/functions/postgres.rs:145-173`
- Test: `core/functions/postgres.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `fn gcd_inner(a: i64, b: i64) -> Result<i64, LimboError>`, `pub fn exec_gcd(a: i64, b: i64) -> Result<Value, LimboError>`, `pub fn exec_lcm(a: i64, b: i64) -> Result<Value, LimboError>` (signatures change from bare `Value`/`i64` returns).
- Consumes: nothing new; both call sites are in the same file.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn gcd_overflow_raises() {
    assert!(matches!(exec_gcd(i64::MIN, 0), Err(LimboError::IntegerOverflow)));
    assert!(matches!(exec_gcd(0, i64::MIN), Err(LimboError::IntegerOverflow)));
    assert!(matches!(exec_gcd(i64::MIN, i64::MIN), Err(LimboError::IntegerOverflow)));
    // Euclid's algorithm reaches i64::MIN % -1 mid-loop for this pair even
    // though neither input matches the 3 previously special-cased tuples.
    // Before the fix this panics the process instead of returning Err.
    assert!(matches!(exec_gcd(i64::MIN, -1), Err(LimboError::IntegerOverflow)));
    assert!(matches!(exec_gcd(-1, i64::MIN), Err(LimboError::IntegerOverflow)));
}

#[test]
fn lcm_overflow_raises_on_min_abs() {
    // b.wrapping_abs() previously silently wrapped i64::MIN back to i64::MIN.
    assert!(matches!(exec_lcm(i64::MIN, -1), Err(LimboError::IntegerOverflow)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_core --lib functions::postgres::tests::gcd_overflow_raises`
Expected: test panics (SIGILL/overflow trap) rather than failing cleanly — this itself is the bug.

- [ ] **Step 3: Implement**

```rust
fn gcd_inner(mut a: i64, mut b: i64) -> Result<i64, LimboError> {
    while b != 0 {
        let t = b;
        b = a.checked_rem(b).ok_or(LimboError::IntegerOverflow)?;
        a = t;
    }
    a.checked_abs().ok_or(LimboError::IntegerOverflow)
}

pub fn exec_gcd(a: i64, b: i64) -> Result<Value, LimboError> {
    Ok(Value::from_i64(gcd_inner(a, b)?))
}

pub fn exec_lcm(a: i64, b: i64) -> Result<Value, LimboError> {
    if a == 0 || b == 0 {
        return Ok(Value::from_i64(0));
    }
    let g = gcd_inner(a, b)?; // g is always > 0, so a/g never hits MIN/-1
    let b_abs = b.checked_abs().ok_or(LimboError::IntegerOverflow)?;
    let product = (a / g).checked_mul(b_abs).ok_or(LimboError::IntegerOverflow)?;
    product.checked_abs().map(Value::from_i64).ok_or(LimboError::IntegerOverflow)
}
```

`i64::MIN % -1` / `i64::MIN / -1` panic unconditionally in Rust in both debug and release (remainder/division overflow always traps, unlike add/sub/mul). Replacing the ad hoc tuple pre-checks with `checked_rem`/`checked_abs`/`checked_mul` covers every overflow path generally. Update both call sites in `core/vdbe/execute.rs` (same match block already using `?` for these two functions today, so this is call-site compatible).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_core --lib functions::postgres::tests::gcd_overflow_raises functions::postgres::tests::lcm_overflow_raises_on_min_abs`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/functions/postgres.rs
git commit -S -m "fix(functions): prevent gcd/lcm integer-overflow panic on i64::MIN"
```

---

### Task E2: Cap repeat()/lpad()/rpad() to prevent unbounded-allocation DoS (C2)

**Files:**
- Modify: `core/functions/postgres.rs:121-143,176-186,334-356`
- Modify: `core/vdbe/execute.rs:7205-7266` (call-site `?` propagation — **must land in the same commit or the build breaks**)
- Test: `core/functions/postgres.rs` (inline `mod tests`)

**Interfaces:**
- Produces: `const PG_MAX_STRING_LEN: usize = 1_073_741_823`, `fn check_pg_string_length(len: usize) -> Result<(), LimboError>`, `pub fn exec_repeat(input: &Value, count: i64) -> Result<Value, LimboError>`, `pub fn exec_lpad(input: &Value, length: usize, fill: &str) -> Result<Value, LimboError>`, `pub fn exec_rpad(...) -> Result<Value, LimboError>` (same pattern as `exec_lpad`).
- Consumes: `LimboError::InvalidArgument(String)` (`core/error.rs:43`, existing variant).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn repeat_rejects_oversized_result() {
    let err = exec_repeat(&Value::build_text("x"), 2_000_000_000).unwrap_err();
    assert!(matches!(err, LimboError::InvalidArgument(_)));
}

#[test]
fn lpad_rejects_oversized_length() {
    let err = exec_lpad(&Value::build_text("x"), 2_000_000_000, " ").unwrap_err();
    assert!(matches!(err, LimboError::InvalidArgument(_)));
}

#[test]
fn rpad_rejects_oversized_length() {
    let err = exec_rpad(&Value::build_text("x"), 2_000_000_000, " ").unwrap_err();
    assert!(matches!(err, LimboError::InvalidArgument(_)));
}

#[test]
fn repeat_still_works_under_cap() {
    assert_eq!(exec_repeat(&Value::build_text("ab"), 3).unwrap(), Value::build_text("ababab"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_core --lib functions::postgres::tests::repeat_rejects_oversized_result`
Expected: FAIL to compile (`exec_repeat` still returns bare `Value`, not `Result`) — confirms the test targets the pre-fix signature.

- [ ] **Step 3: Implement**

```rust
/// PostgreSQL's varlena MaxAllocSize (1GB - 1 byte). repeat()/lpad()/rpad()
/// raise "requested length too large" instead of allocating past this.
const PG_MAX_STRING_LEN: usize = 1_073_741_823;

fn check_pg_string_length(len: usize) -> Result<(), LimboError> {
    if len > PG_MAX_STRING_LEN {
        return Err(LimboError::InvalidArgument("requested length too large".to_string()));
    }
    Ok(())
}

pub fn exec_repeat(input: &Value, count: i64) -> Result<Value, LimboError> {
    let s = match input {
        Value::Text(t) => t.as_str(),
        Value::Null => return Ok(Value::Null),
        _ => return Ok(Value::Null),
    };
    if count <= 0 {
        return Ok(Value::build_text(String::new()));
    }
    let total_len = s
        .len()
        .checked_mul(count as usize)
        .ok_or_else(|| LimboError::InvalidArgument("requested length too large".to_string()))?;
    check_pg_string_length(total_len)?;
    Ok(Value::build_text(s.repeat(count as usize)))
}

pub fn exec_lpad(input: &Value, length: usize, fill: &str) -> Result<Value, LimboError> {
    check_pg_string_length(length)?;
    let s = match input {
        Value::Text(t) => t.to_string(),
        Value::Null => return Ok(Value::Null),
        v => v.to_string(),
    };
    let char_count = s.chars().count();
    if char_count >= length {
        Ok(Value::build_text(s.chars().take(length).collect::<String>()))
    } else {
        let fill_chars: Vec<char> = fill.chars().collect();
        if fill_chars.is_empty() {
            Ok(Value::build_text(s))
        } else {
            let pad: String = fill_chars.iter().cycle().take(length - char_count).collect();
            Ok(Value::build_text(format!("{pad}{s}")))
        }
    }
}
// exec_rpad: identical pattern, `format!("{s}{pad}")` order.
```

`core/vdbe/execute.rs:7219,7235,7265`:

```rust
// before
state.registers[*dest].set_value(exec_lpad(input, length, &fill));
state.registers[*dest].set_value(exec_rpad(input, length, &fill));
state.registers[*dest].set_value(exec_repeat(input, count));
```
```rust
// after
state.registers[*dest].set_value(exec_lpad(input, length, &fill)?);
state.registers[*dest].set_value(exec_rpad(input, length, &fill)?);
state.registers[*dest].set_value(exec_repeat(input, count)?);
```

(`?` is already used in this same match block for `exec_gcd`/`exec_lcm` — mechanical, low-risk change consistent with existing patterns.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_core --lib functions::postgres::tests::repeat_rejects_oversized_result functions::postgres::tests::lpad_rejects_oversized_length functions::postgres::tests::rpad_rejects_oversized_length functions::postgres::tests::repeat_still_works_under_cap && cargo build -p turso_core`
Expected: PASS, build succeeds (confirms `core/vdbe/execute.rs` call sites were updated correctly)

- [ ] **Step 5: Commit**

```bash
git add core/functions/postgres.rs core/vdbe/execute.rs
git commit -S -m "fix(functions): cap repeat/lpad/rpad at PG MaxAllocSize to prevent OOM DoS"
```

---

### Task E3: Add depth guard to hand-rolled JSON validator (C3)

**Files:**
- Modify: `core/pg_catalog.rs:4128-4296`
- Test: `core/pg_catalog.rs` (existing `#[cfg(test)] mod tests` near line 4854)

**Interfaces:**
- Produces: `const MAX_JSON_VALIDATE_DEPTH: usize = 1000`; `parse_json_value`/`parse_json_object`/`parse_json_array` gain a `depth: usize` parameter.
- Consumes: none. Public signature `fn is_valid_json(s: &str) -> bool` (sole call site `core/pg_catalog.rs:3974`) is unchanged.

Do **not** delegate to `crate::json::is_json_valid` (`core/json/mod.rs:836`) — it takes `impl AsValueRef` and returns a tri-state `Value`, a different call shape than this `&str -> bool` validator's one call site warrants. Mirror `core/json/jsonb.rs:16`'s existing `MAX_JSON_DEPTH = 1000` precedent locally instead.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_is_valid_json_rejects_excessive_nesting() {
    // 1001 levels of nested arrays would previously blow the stack before
    // ever returning false; it must now return false without crashing.
    let mut deeply_nested = "[".repeat(1001);
    deeply_nested.push_str(&"]".repeat(1001));
    assert!(!is_valid_json(&deeply_nested));

    // exactly at the limit must still validate successfully.
    let mut at_limit = "[".repeat(1000);
    at_limit.push_str(&"]".repeat(1000));
    assert!(is_valid_json(&at_limit));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_core --lib pg_catalog::tests::test_is_valid_json_rejects_excessive_nesting`
Expected: stack overflow (process abort) before the fix — confirms the crash, not a clean assertion failure.

- [ ] **Step 3: Implement**

```rust
/// Mirrors core/json/jsonb.rs's MAX_JSON_DEPTH — stops adversarial input
/// from blowing the stack via unbounded object/array recursion.
const MAX_JSON_VALIDATE_DEPTH: usize = 1000;

fn is_valid_json(s: &str) -> bool {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return false;
    }
    parse_json_value(trimmed, 0, 0).is_some_and(|end| end == trimmed.len())
}

fn parse_json_value(s: &str, start: usize, depth: usize) -> Option<usize> {
    if depth > MAX_JSON_VALIDATE_DEPTH {
        return None;
    }
    let pos = skip_json_whitespace(s, start);
    if pos >= s.len() {
        return None;
    }
    match s.as_bytes()[pos] {
        b'{' => parse_json_object(s, pos, depth + 1),
        b'[' => parse_json_array(s, pos, depth + 1),
        b'"' => parse_json_string(s, pos),
        b't' => parse_json_literal(s, pos, "true"),
        b'f' => parse_json_literal(s, pos, "false"),
        b'n' => parse_json_literal(s, pos, "null"),
        b'0'..=b'9' | b'-' => parse_json_number(s, pos),
        _ => None,
    }
}

fn parse_json_object(s: &str, start: usize, depth: usize) -> Option<usize> {
    let mut pos = start + 1;
    pos = skip_json_whitespace(s, pos);
    if s.as_bytes().get(pos) == Some(&b'}') {
        return Some(pos + 1);
    }
    loop {
        if s.as_bytes().get(pos) != Some(&b'"') { return None; }
        pos = parse_json_string(s, pos)?;
        pos = skip_json_whitespace(s, pos);
        if s.as_bytes().get(pos) != Some(&b':') { return None; }
        pos += 1;
        pos = parse_json_value(s, pos, depth)?;
        pos = skip_json_whitespace(s, pos);
        match s.as_bytes().get(pos) {
            Some(b',') => { pos += 1; pos = skip_json_whitespace(s, pos); }
            Some(b'}') => return Some(pos + 1),
            _ => return None,
        }
    }
}

fn parse_json_array(s: &str, start: usize, depth: usize) -> Option<usize> {
    let mut pos = start + 1;
    pos = skip_json_whitespace(s, pos);
    if s.as_bytes().get(pos) == Some(&b']') {
        return Some(pos + 1);
    }
    loop {
        pos = parse_json_value(s, pos, depth)?;
        pos = skip_json_whitespace(s, pos);
        match s.as_bytes().get(pos) {
            Some(b',') => { pos += 1; pos = skip_json_whitespace(s, pos); }
            Some(b']') => return Some(pos + 1),
            _ => return None,
        }
    }
}
```

(`parse_json_string`/`parse_json_number`/`parse_json_literal` are leaf parsers, unchanged — no recursion.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_core --lib pg_catalog::tests::test_is_valid_json_rejects_excessive_nesting`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs
git commit -S -m "fix(pg_catalog): guard JSON validator recursion depth to prevent stack overflow"
```

---

### Task E-dead-tests: Fix two dead catalog tests that assert nothing (Low, land early as sanity net)

**Files:**
- Modify: `tests/integration/postgres/catalog.rs:39-99` (`test_postgres_pg_class`, `test_postgres_pg_attribute`)

Both tests predate the real pg_class/pg_attribute implementation — their stale comments literally say "haven't implemented yet" even though the mapping has since been built. Zero prerequisites; land first as a self-verifying sanity net for E4/E8 below.

- [ ] **Step 1: Replace both tests with real assertions**

```rust
#[turso_macros::test]
fn test_postgres_pg_class(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare("SELECT relname, relkind FROM pg_class WHERE relkind = 'r'").unwrap();
    let mut found_users_table = false;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                let row = stmt.row().unwrap();
                if let (Value::Text(relname), Value::Text(relkind)) = (row.get_value(0), row.get_value(1)) {
                    if relname.as_str() == "users" && relkind.as_str() == "r" {
                        found_users_table = true;
                    }
                }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(found_users_table, "users table not found in pg_class");
}

#[turso_macros::test]
fn test_postgres_pg_attribute(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare("SELECT COUNT(*) FROM pg_attribute").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            let Value::Numeric(Numeric::Integer(count)) = row.get_value(0) else {
                panic!("expected integer count");
            };
            assert_eq!(*count, 2, "pg_attribute should have 2 rows for users(id, name)");
        }
        _ => panic!("Expected row from COUNT query"),
    }
}
```

- [ ] **Step 2: Run to verify pass**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_postgres_pg_class integration::postgres::catalog::test_postgres_pg_attribute`
Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add tests/integration/postgres/catalog.rs
git commit -S -m "test(catalog): assert real pg_class/pg_attribute mapping instead of stale placeholders"
```

---

### Task E4: Fix indisprimary misclassifying UNIQUE-backed sqlite_autoindex as PRIMARY KEY (H25)

**Files:**
- Modify: `core/pg_catalog.rs:2874-3040` (`PgIndexCursor::load_indexes`)
- Test: `tests/integration/postgres/catalog.rs`

**Interfaces:** shares the column-position-set-matching technique with Task E10 (H31) — same engineer, separate commits.

- [ ] **Step 1: Write the failing test**

```rust
#[turso_macros::test]
fn test_pg_index_indisprimary_distinguishes_pk_from_unique(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn.prepare(
        "SELECT indisprimary, indisunique FROM pg_index
         JOIN pg_class c ON c.oid = indrelid
         WHERE c.relname = 't' ORDER BY indisprimary DESC",
    ).unwrap();

    let mut pk_count = 0;
    let mut rows = 0;
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => {
                rows += 1;
                if stmt.row().unwrap().get_value(0).as_int() == Some(1) { pk_count += 1; }
            }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert_eq!(rows, 2, "expected 2 auto-indexes (pk + unique email)");
    assert_eq!(pk_count, 1, "exactly one index should be marked indisprimary");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_index_indisprimary_distinguishes_pk_from_unique`
Expected: FAIL, `pk_count == 2` (both indexes misclassified as primary via name-prefix match)

- [ ] **Step 3: Implement**

```rust
// before
for (table_name, _) in &tables {
    let table_oid = tbl_oid_map.get(*table_name).copied().unwrap_or(0);
    for idx in schema.get_indices(table_name) {
        if idx.ephemeral { continue; }
        let indnatts = idx.columns.len() as i64;
        let indisunique = i64::from(idx.unique);
        let indisprimary = i64::from(
            idx.name.starts_with(PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX) && idx.unique,
        );
        // ...
    }
}
```
```rust
// after
for (table_name, table) in &tables {
    let table_oid = tbl_oid_map.get(*table_name).copied().unwrap_or(0);
    let Table::BTree(btree) = table.as_ref() else { continue };
    for idx in schema.get_indices(table_name) {
        if idx.ephemeral { continue; }
        let indnatts = idx.columns.len() as i64;
        let indisunique = i64::from(idx.unique);

        // Resolve this index's column-position set (skipping expression
        // columns, which can't back a PK) and compare against unique_sets
        // entries flagged is_primary_key — authoritative, unlike matching
        // on the ambiguous sqlite_autoindex_* name prefix which SQLite
        // also uses for plain UNIQUE constraints.
        let idx_positions: std::collections::BTreeSet<usize> = idx
            .columns.iter().filter(|c| c.expr.is_none()).map(|c| c.pos_in_table).collect();
        let indisprimary = i64::from(btree.unique_sets.iter().any(|us| {
            us.is_primary_key
                && us.columns.iter().all(|(name, _)| {
                    btree.get_column(name).is_some_and(|(pos, _)| idx_positions.contains(&pos))
                })
                && us.columns.len() == idx_positions.len()
        }));
        // ...
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_index_indisprimary_distinguishes_pk_from_unique`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(pg_catalog): distinguish PK index from UNIQUE index via column-position match"
```

---

### Task E5 (not scheduled — design gap): Stable pg_class/pg_attribute OIDs (H24)

**Do not implement as written.** `core/pg_catalog.rs:148-156`'s `table_oid_map` assigns OIDs by table iteration position; every downstream consumer (`PgIndexCursor`, `PgConstraintCursor`, `pg_get_constraintdef`, `pg_get_indexdef`) assumes table OIDs occupy a contiguous `[USER_TABLE_OID_START, USER_TABLE_OID_START + num_tables)` block and continues counting from `num_tables` for index/constraint OIDs. Replacing table OIDs with a hash-based `stable_table_oid()` (shown below for reference) without also redesigning the index/constraint OID allocation risks collisions between table and index/constraint OID spaces.

```rust
// reference implementation — do not land without the broader OidAllocator redesign
fn stable_table_oid(name: &str, used: &std::collections::HashSet<i64>) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    let mut oid = USER_TABLE_OID_START + (hasher.finish() % 1_000_000) as i64;
    while used.contains(&oid) { oid += 1; }
    oid
}
```

**Recommended scope:** combine with the Medium "triple `USER_TABLE_OID_START`" finding (Task E-medium-oid-const below) into one `OidAllocator` design spike — a single global counter/registry in `pg_catalog.rs`, exported `pub(crate)` to `pg_comment.rs`, handing out OIDs for tables/indexes/constraints/functions from one non-overlapping space. This is size **L**, not the M estimated by naive per-finding scoping — schedule as its own planning pass, not inside this task list.

---

### Task E-attached-schema (investigation spike — do not implement yet): pg_class/pg_attribute visibility for CREATE SCHEMA tables (H26)

**Files:** `core/pg_catalog.rs:324-453` (`PgClassCursor::load_from_sqlite_master`), `core/pg_catalog.rs:769-824` (`PgAttributeTable::load_attributes`), likely `core/lib.rs:2698-2762` (`DatabaseCatalog`).

Both cursors only ever read `self.conn.schema` — the **main** database's schema. `CREATE SCHEMA` attaches a separate SQLite file as a named database (`core/connection.rs`); its tables live in a different `Schema` object neither cursor touches. `DatabaseCatalog` currently exposes no `pub` accessor for an attached database's own `Schema` (`get_database_by_name` etc. at `core/lib.rs:2715-2736` are private). **Spike required before this is a scoped task:** design a `pub(crate) fn schema_by_name(&self, name: &str) -> Option<Arc<RwLock<Schema>>>`-shaped accessor on `DatabaseCatalog`/`Database`, confirm where `Database`'s own `Schema` field lives (`core/lib.rs:685`, not yet located precisely), then re-scope as S/M. Do not attempt this as a bite-sized task until the spike lands.

---

### Task E6: Fix pronamespace hardcoded to public(2200) for built-in functions (H27)

**Files:**
- Modify: `core/pg_catalog.rs:1483-1526` (builtin loop), `~1541` (alias loop), `~1580` (extension loop), all inside `PgProcTable`/`PgProcCursor::load_functions`
- Test: `tests/integration/postgres/catalog.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[turso_macros::test]
fn test_pg_proc_builtin_functions_in_pg_catalog_namespace(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare("SELECT pronamespace FROM pg_proc WHERE proname = 'lower'").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let ns = stmt.row().unwrap().get_value(0).as_int().unwrap();
            assert_eq!(ns, 11, "built-in function lower() should be in pg_catalog (oid 11), not public");
        }
        _ => panic!("lower() not found in pg_proc"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_proc_builtin_functions_in_pg_catalog_namespace`
Expected: FAIL, `ns == 2200`

- [ ] **Step 3: Implement**

At all 3 sites, replace `Value::from_i64(2200), // pronamespace (public)` with `Value::from_i64(11), // pronamespace (pg_catalog) — built-ins live in pg_catalog like real PostgreSQL, not public.` This does **not** apply to `pg_class`/`pg_attribute`'s `relnamespace` or `pg_constraint`'s `connamespace` — those stay `2200` for user tables (fixed properly under the H26 spike above).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_proc_builtin_functions_in_pg_catalog_namespace`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(pg_catalog): built-in functions belong in pg_catalog namespace, not public"
```

---

### Task E7: Derive attlen/attbyval from resolved PG type (H28) — bundle attalign (Low)

**Files:**
- Modify: `core/pg_catalog.rs:769-824` (`PgAttributeTable::load_attributes`), reuses `PG_BASE_TYPES` (`core/pg_catalog.rs:2357`)
- Test: `tests/integration/postgres/catalog.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[turso_macros::test]
fn test_pg_attribute_varlena_columns_not_byval(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare(
        "SELECT attlen, attbyval, attalign FROM pg_attribute
         JOIN pg_class c ON c.oid = attrelid
         WHERE c.relname = 't' AND attname = 'name'",
    ).unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let row = stmt.row().unwrap();
            assert_eq!(row.get_value(0).as_int(), Some(-1), "text is varlena, attlen must be -1");
            assert_eq!(row.get_value(1).as_int(), Some(0), "text is pass-by-reference, attbyval must be false");
        }
        _ => panic!("column 'name' not found in pg_attribute"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_attribute_varlena_columns_not_byval`
Expected: passes on attlen (already -1 by hardcoding) but FAILs on attbyval (hardcoded `1`/true regardless of type)

- [ ] **Step 3: Implement**

```rust
fn pg_type_info_for_oid(oid: i64) -> Option<&'static PgTypeInfo> {
    PG_BASE_TYPES.iter().find(|t| t.oid == oid)
}
// inside the per-column loop, after `let type_oid = sqlite_type_to_pg_oid(&col.ty_str);`
let (attlen, attbyval, attalign) = pg_type_info_for_oid(type_oid)
    .map(|t| (t.typlen, t.typbyval, t.typalign))
    .unwrap_or((-1, false, "i")); // unknown/array/enum: varlena, pass-by-reference
// replace hardcoded attlen/attbyval/attalign pushes with `attlen`, `i64::from(attbyval)`, `attalign` respectively.
```

`attstorage`/`attidentity`/`attgenerated` stay hardcoded — documented known gap, they need `PgTypeInfo.typstorage` wiring and identity/generated-column tracking not currently modeled in `Column`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_attribute_varlena_columns_not_byval`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(pg_catalog): derive attlen/attbyval/attalign from resolved PG type"
```

---

### Task E8: Exclude hidden/generated columns from pg_attribute and relnatts (H29)

**Files:**
- Modify: `core/pg_catalog.rs:769-824` (`PgAttributeTable::load_attributes`) and `PgClassCursor`'s `relnatts` computation (grep for `relnatts` in the same file — exact second site not pinned down by the review, confirm before implementing)
- Test: `tests/integration/postgres/catalog.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[turso_macros::test]
fn test_pg_attribute_excludes_hidden_columns(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare(
        "SELECT COUNT(*) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid WHERE c.relname = 't'",
    ).unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let count = stmt.row().unwrap().get_value(0).as_int().unwrap();
            assert_eq!(count, 2, "pg_attribute row count must match visible column count only");
        }
        _ => panic!("count query failed"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_attribute_excludes_hidden_columns`
Expected: this specific 2-column case likely passes today (no hidden columns in this table); this test is a regression guard — add a second assertion once a concrete hidden-column repro is confirmed by the implementing engineer (STRICT/WITHOUT ROWID or generated-column table). Do not skip verifying the `col.hidden()` branch is actually exercised before landing.

- [ ] **Step 3: Implement**

```rust
// before
for (i, col) in columns.iter().enumerate() {
    let col_name = col.name.clone().unwrap_or_default();
    // ... uses (i + 1) as attnum unconditionally
```
```rust
// after
let mut attnum = 0i64;
for col in columns.iter() {
    if col.hidden() { continue; }
    attnum += 1;
    let col_name = col.name.clone().unwrap_or_default();
    // ... use `attnum` in place of `(i + 1)` for the attnum field
```

Apply the same `col.hidden()` filter to `PgClassCursor`'s `relnatts` count — grep `relnatts` in `core/pg_catalog.rs` to find that site.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_attribute_excludes_hidden_columns`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(pg_catalog): exclude hidden/generated columns from pg_attribute and relnatts"
```

---

### Task E9: Fix prokind mislabeling aggregates as window functions (H30)

**Files:**
- Modify: `core/pg_catalog.rs:1483-1526` (`PgProcCursor::load_functions`, builtin loop)
- Test: `tests/integration/postgres/catalog.rs`

Root cause is upstream and intentional: `core/function.rs:1768-1773` tags every `AggFunc` entry `func_type: "w"` for SQLite's own `PRAGMA function_list` consumer — not itself a bug, must not regress. Fix stays local to `pg_catalog.rs`.

- [ ] **Step 1: Write the failing test**

```rust
#[turso_macros::test]
fn test_pg_proc_aggregate_prokind_is_a(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare("SELECT prokind FROM pg_proc WHERE proname = 'sum'").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let Value::Text(prokind) = stmt.row().unwrap().get_value(0) else { panic!("expected text prokind") };
            assert_eq!(prokind.as_str(), "a", "sum() is an aggregate, must report prokind='a'");
        }
        _ => panic!("sum() not found in pg_proc"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_proc_aggregate_prokind_is_a`
Expected: FAIL, `prokind == "w"`

- [ ] **Step 3: Implement**

Before implementing, confirm `AggFunc`'s exact name-accessor method (`core/function.rs` around line 289 — `AggFunc::iter()` is confirmed public; the string-name accessor is not confirmed by this review, verify before writing the code below).

```rust
// before (inside the builtin loop, ~1488-1492)
let prokind = match entry.func_type {
    "a" => "a", // dead: AggFunc entries carry "w", never "a"
    "w" => "w",
    _ => "f",
};
```
```rust
// after
use crate::function::AggFunc;
use std::collections::HashSet;
// hoisted once per load_functions() call, not per-row:
let agg_names: HashSet<&'static str> = AggFunc::iter().map(|f| f.static_name()).collect();
// ...
let prokind = if agg_names.contains(entry.name.as_str()) {
    "a" // aggregate
} else {
    match entry.func_type {
        "w" => "w", // genuine window function
        _ => "f",
    }
};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_proc_aggregate_prokind_is_a`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs
git commit -S -m "fix(pg_catalog): report prokind='a' for aggregates instead of 'w'"
```

---

### Task E10: Fix conindid resolved via unscoped substring match (H31)

**Files:**
- Modify: `core/pg_catalog.rs:3107-3254` (`PgConstraintCursor::load_constraints`, PK/UNIQUE block)
- Test: `tests/integration/postgres/catalog.rs`

Same column-position-set-matching technique as Task E4 (H25) — same engineer, separate commit (different cursor).

- [ ] **Step 1: Write the failing test**

```rust
#[turso_macros::test]
fn test_pg_constraint_conindid_scoped_to_own_table(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    conn.execute("CREATE TABLE accounts (id INTEGER PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    let mut stmt = conn.prepare(
        "SELECT con.conindid, idx.indrelid = con.conrelid AS same_table
         FROM pg_constraint con
         JOIN pg_index idx ON idx.indexrelid = con.conindid
         JOIN pg_class c ON c.oid = con.conrelid
         WHERE c.relname = 'users' AND con.contype = 'u'",
    ).unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let same_table = stmt.row().unwrap().get_value(1).as_int();
            assert_eq!(same_table, Some(1), "conindid must reference an index on the constraint's own table");
        }
        _ => panic!("UNIQUE constraint on users.email not found"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_constraint_conindid_scoped_to_own_table`
Expected: FAIL or flaky depending on HashMap iteration order — `index_oid_map.iter().find(...)` unscoped substring search can match `accounts`'s index for `users`'s constraint.

- [ ] **Step 3: Implement**

```rust
// before (lines ~3206-3224)
let conindid = if us.is_primary_key {
    index_oid_map.iter()
        .find(|(k, _)| k.starts_with(&format!("{PRIMARY_KEY_AUTOMATIC_INDEX_NAME_PREFIX}{table_name}")))
        .map(|(_, &v)| v).unwrap_or(0)
} else {
    index_oid_map.iter()
        .find(|(k, _)| k.contains(&col_names.join("_")))
        .map(|(_, &v)| v).unwrap_or(0)
};
```
```rust
// after
// Resolve conindid by matching this constraint's column-position set against
// schema.get_indices(table_name) directly (scoped to the current table),
// instead of a global substring search over ALL tables' index names.
let col_positions: Vec<usize> = col_names.iter()
    .filter_map(|name| btree.get_column(name).map(|(pos, _)| pos))
    .collect();
let conindid = schema.get_indices(table_name)
    .find(|idx| {
        !idx.ephemeral && idx.unique
            && idx.columns.iter().filter(|c| c.expr.is_none()).map(|c| c.pos_in_table)
                .collect::<std::collections::BTreeSet<_>>()
                == col_positions.iter().copied().collect::<std::collections::BTreeSet<_>>()
    })
    .and_then(|idx| index_oid_map.get(&idx.name).copied())
    .unwrap_or(0);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_constraint_conindid_scoped_to_own_table`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(pg_catalog): scope conindid resolution to the constraint's own table"
```

---

### Task E11: Prevent regex-based DDL conversion from corrupting string literals (H32) — bundle trailing-space fix (Low)

**Files:**
- Modify: `core/pg_catalog.rs:4298-4346` (regexes + `convert_sqlite_ddl_to_postgres`)
- Test: `core/pg_catalog.rs` inline `#[cfg(test)] mod tests`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_convert_ddl_preserves_string_literals() {
    let ddl = "CREATE TABLE t (id INTEGER PRIMARY KEY, kind TEXT DEFAULT 'REAL ESTATE', note TEXT CHECK (note != 'BLOB storage'))";
    let converted = convert_sqlite_ddl_to_postgres(ddl);
    assert!(converted.contains("'REAL ESTATE'"), "string literal must survive verbatim, got: {converted}");
    assert!(converted.contains("'BLOB storage'"), "string literal must survive verbatim, got: {converted}");
    assert!(converted.contains("kind text"));
    assert!(converted.contains("note text"));
}

#[test]
fn test_convert_ddl_no_trailing_space_after_serial_pk() {
    let ddl = "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)";
    let converted = convert_sqlite_ddl_to_postgres(ddl);
    assert!(
        converted.contains("SERIAL PRIMARY KEY)") || converted.contains("SERIAL PRIMARY KEY,"),
        "expected no stray space/double-space after SERIAL PRIMARY KEY, got: {converted}"
    );
    assert!(!converted.contains("SERIAL PRIMARY KEY  "), "double space found: {converted}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_core --lib pg_catalog::tests::test_convert_ddl_preserves_string_literals pg_catalog::tests::test_convert_ddl_no_trailing_space_after_serial_pk`
Expected: FAIL — `'REAL ESTATE'` becomes `'double precision ESTATE'` under the pre-fix regex-everywhere approach; trailing space also present.

- [ ] **Step 3: Implement**

```rust
static SQLITE_DDL_STRING_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"'(?:[^']|'')*'").expect("valid string literal regex"));

static SQLITE_DDL_AUTOINCREMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*\bAUTOINCREMENT\b").expect("valid AUTOINCREMENT regex"));
// (consumes a preceding whitespace run along with the keyword, so
// "...KEY AUTOINCREMENT)" -> "...KEY)" instead of "...KEY )")

fn convert_ddl_segment(segment: &str) -> String {
    let mut s = segment.to_string();
    s = SQLITE_DDL_INTEGER_PK.replace_all(&s, "SERIAL PRIMARY KEY").into_owned();
    s = SQLITE_DDL_AUTOINCREMENT.replace_all(&s, "").into_owned();
    s = SQLITE_DDL_INTEGER.replace_all(&s, "integer").into_owned();
    s = SQLITE_DDL_REAL.replace_all(&s, "double precision").into_owned();
    s = SQLITE_DDL_TEXT.replace_all(&s, "text").into_owned();
    s = SQLITE_DDL_BLOB.replace_all(&s, "bytea").into_owned();
    s = SQLITE_DDL_DATETIME.replace_all(&s, "timestamp").into_owned();
    s = SQLITE_DDL_WITHOUT_ROWID.replace_all(&s, "").into_owned();
    s
}

/// Rewrites SQLite DDL keywords to their PostgreSQL equivalents, skipping
/// over '...' string literals (e.g. inside DEFAULT/CHECK expressions) so
/// e.g. DEFAULT 'REAL ESTATE' isn't corrupted into 'double precision ESTATE'.
fn convert_sqlite_ddl_to_postgres(sqlite_ddl: &str) -> String {
    let mut result = String::with_capacity(sqlite_ddl.len());
    let mut last_end = 0;
    for m in SQLITE_DDL_STRING_LITERAL.find_iter(sqlite_ddl) {
        result.push_str(&convert_ddl_segment(&sqlite_ddl[last_end..m.start()]));
        result.push_str(m.as_str()); // literal preserved verbatim
        last_end = m.end();
    }
    result.push_str(&convert_ddl_segment(&sqlite_ddl[last_end..]));
    result
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_core --lib pg_catalog::tests::test_convert_ddl_preserves_string_literals pg_catalog::tests::test_convert_ddl_no_trailing_space_after_serial_pk`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add core/pg_catalog.rs
git commit -S -m "fix(pg_catalog): skip string literals when converting SQLite DDL to PostgreSQL"
```

---

### Task E-medium-arity: Key stable_proc_oid_map by name+arity, not name alone

**Files:** Modify: `core/pg_catalog.rs:1456-1474` (`stable_proc_oid_map`) and consumers (`load_functions`, ~1483-1526) | Test: `tests/integration/postgres/catalog.rs`

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_pg_proc_overloaded_builtin_has_distinct_oids(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare("SELECT DISTINCT oid FROM pg_proc WHERE proname = 'round'").unwrap();
    let mut oids = std::collections::HashSet::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => { oids.insert(stmt.row().unwrap().get_value(0).as_int()); }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert!(oids.len() >= 2, "round/1 and round/2 must get distinct OIDs, got {}", oids.len());
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_proc_overloaded_builtin_has_distinct_oids` — Expected: FAIL, `oids.len() == 1`.

- [ ] **Step 3: Implement**

```rust
fn stable_proc_oid_map(conn: &Connection) -> HashMap<(String, i32), i64> {
    use crate::function::Func;
    let mut keys: Vec<(String, i32)> = Func::builtin_function_list().into_iter()
        .chain(Func::pg_proc_alias_entries())
        .flat_map(|e| e.arities.iter().map(move |&a| (e.name.clone(), a)))
        .collect();
    for (name, arity, _) in conn.get_syms_functions() { keys.push((name, arity)); }
    keys.sort();
    keys.dedup();
    keys.into_iter().enumerate().map(|(i, key)| (key, PG_PROC_OID_BASE + i as i64)).collect()
}
```

Every call site doing `oid_map.get(&entry.name)` must look up per-arity, emitting one `pg_proc` row per arity (matches real PostgreSQL, one row per overload). **Before implementing, verify `FunctionListEntry`'s exact arities field name/type** — assumed `arities: &'static [i32]` from context, not directly confirmed.

- [ ] **Step 4: Verify pass** — same command as Step 2, expect PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): key stable_proc_oid_map by name+arity to avoid overload collisions"`

---

### Task E-medium-oid-const: Single shared OID base constant

**Files:** Modify: `core/pg_catalog.rs:16` (`const USER_TABLE_OID_START` → `pub(crate) const`), `core/pg_comment.rs:12` (delete duplicate, `use crate::pg_catalog::USER_TABLE_OID_START;`)

- [ ] **Step 1:** Change `core/pg_catalog.rs:16` to `pub(crate) const USER_TABLE_OID_START: i64 = 16384;`
- [ ] **Step 2:** Delete the duplicate at `core/pg_comment.rs:12`, add `use crate::pg_catalog::USER_TABLE_OID_START;`
- [ ] **Step 3:** Run: `cargo build -p turso_core` — Expected: builds clean (this is a pure de-dup, no behavior test needed beyond compilation + existing catalog tests still passing: `cargo test -p core_tester --test integration_tests integration::postgres::catalog`)
- [ ] **Step 4: Commit** — `git commit -S -m "refactor(pg_catalog): share USER_TABLE_OID_START constant with pg_comment.rs"`

Distinct from Task E5 (H24)'s full OID-stability redesign — do not conflate. Depends on nothing; can land anytime. Task E-medium-oid-map below depends on this landing first (or land together).

---

### Task E-medium-oid-map: Extract shared table_oid_map so pg_comment.rs stops drifting (case-collision bug)

**Files:** Modify: `core/pg_catalog.rs:148-156` (make `table_oid_map`/`user_tables_sorted` `pub(crate)`), `core/pg_comment.rs:63-96` (delete local copies, call shared versions) | Test: `tests/integration/postgres/catalog.rs`

`pg_comment.rs`'s local copy lowercases its OID map keys (`.to_lowercase()`); `pg_catalog.rs`'s does not — a mixed-case table name resolves to different OIDs in the two files, breaking `pg_description` joins against `pg_class.oid`.

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_comment_on_mixed_case_table_resolves_same_oid_as_pg_class(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE \"MixedCase\" (id INTEGER PRIMARY KEY)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("COMMENT ON TABLE \"MixedCase\" IS 'a table'").unwrap();
    let mut stmt = conn.prepare(
        "SELECT d.description FROM pg_description d JOIN pg_class c ON c.oid = d.objoid WHERE c.relname = 'MixedCase'",
    ).unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {
            let Value::Text(desc) = stmt.row().unwrap().get_value(0) else { panic!("expected text") };
            assert_eq!(desc.as_str(), "a table");
        }
        _ => panic!("pg_description join on pg_class.oid found no row — case-key mismatch"),
    }
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_comment_on_mixed_case_table_resolves_same_oid_as_pg_class` — Expected: FAIL, no row.

- [ ] **Step 3: Implement**

```rust
// core/pg_comment.rs — before
fn user_tables_sorted(schema: &Schema) -> Vec<(&String, &Arc<Table>)> { /* duplicate body */ }
fn table_oid_map(schema: &Schema) -> HashMap<String, i64> {
    user_tables_sorted(schema).into_iter().enumerate()
        .map(|(i, (name, _))| (name.to_lowercase(), USER_TABLE_OID_START + i as i64))
        .collect()
}
fn resolve_table_oid(conn: &Connection, table_name: &str) -> Result<i64> {
    let schema = conn.schema.read();
    let map = table_oid_map(&schema);
    map.get(&table_name.to_lowercase()).copied()
        .ok_or_else(|| LimboError::ParseError(format!("relation \"{table_name}\" does not exist")))
}
```
```rust
// core/pg_comment.rs — after
use crate::pg_catalog::table_oid_map; // now pub(crate) in pg_catalog.rs

fn resolve_table_oid(conn: &Connection, table_name: &str) -> Result<i64> {
    let schema = conn.schema.read();
    let map = table_oid_map(&schema); // keys preserve original case, matching pg_class.oid
    map.get(table_name).copied()
        .ok_or_else(|| LimboError::ParseError(format!("relation \"{table_name}\" does not exist")))
}
```

If case-insensitive lookup is actually needed, that normalization must be added consistently to the shared `table_oid_map` in `pg_catalog.rs` itself (check `map_table_name()` conventions in `translator.rs` first) — not re-introduced unilaterally in `pg_comment.rs`.

- [ ] **Step 4: Verify pass** — same command as Step 2, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_comment): use pg_catalog's shared table_oid_map to stop OID key drift"`

Depends on Task E-medium-oid-const landing first (or together — both touch the same constant's visibility).

---

### Task E-medium-reltype (not scoped as single task): pg_class.reltype hardcoded to 0

Real PostgreSQL creates an implicit composite row type per table and points `pg_class.reltype` at it. `pg_type` currently has no per-table composite-type rows at all. This is a **two-part fix**: (1) `PgTypeCursor::load_types` must synthesize a composite-type row per user table (new OID range, `typtype='c'`, `typrelid=<table oid>`), (2) `PgClassCursor` (`core/pg_catalog.rs:360,417`) then points `reltype` at it. Part 2 alone (M) is meaningless without part 1 — schedule as one combined task, do not implement part 2 in isolation. Test shape for reference once both land:

```rust
#[turso_macros::test]
fn test_pg_class_reltype_matches_pg_type_row(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare(
        "SELECT c.reltype, ty.oid FROM pg_class c LEFT JOIN pg_type ty ON ty.typrelid = c.oid WHERE c.relname = 't'",
    ).unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => assert_ne!(stmt.row().unwrap().get_value(0).as_int(), Some(0), "reltype must not be 0"),
        _ => panic!("t not found in pg_class"),
    }
}
```

---

### Task E-medium-enum-panic: Guard unchecked slice index in parse_enum_labels_from_type_def

**Files:** Modify: `core/pg_catalog.rs:888-908` | Test: `core/pg_catalog.rs` inline `#[cfg(test)] mod tests`

- [ ] **Step 1: Write failing test** (verify `TypeDef`'s actual field list before writing — only `is_builtin`/`sql` confirmed; add `..Default::default()` if it derives `Default`, else fill remaining fields explicitly)

```rust
#[test]
fn test_parse_enum_labels_handles_malformed_type_def() {
    // "AS ENUM" present but ')' appears before '(' — must not panic on the
    // rest[start_paren + 1..end_paren] slice.
    let td = TypeDef { is_builtin: false, sql: "CREATE TYPE mood AS ENUM )(".to_string() };
    let labels = parse_enum_labels_from_type_def(&td);
    assert!(labels.is_empty());
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p turso_core --lib pg_catalog::tests::test_parse_enum_labels_handles_malformed_type_def` — Expected: panic (slice index out of range / start > end).

- [ ] **Step 3: Implement**

```rust
// before
if let (Some(start_paren), Some(end_paren)) = (rest.find('('), rest.rfind(')')) {
    let inner = &rest[start_paren + 1..end_paren];
    return inner.split(',').filter_map(parse_enum_label_token).collect();
}
```
```rust
// after
if let (Some(start_paren), Some(end_paren)) = (rest.find('('), rest.rfind(')')) {
    if start_paren < end_paren {
        let inner = &rest[start_paren + 1..end_paren];
        return inner.split(',').filter_map(parse_enum_label_token).collect();
    }
}
```

- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): guard against panic on malformed ENUM type definition"`

---

### Task E-medium-perf (design spike, not a fix ticket): O(N²) full-schema rescans

Every cursor's `filter()`/`load_*()` does `schema.read().clone()` and rebuilds its full OID map from scratch per query; `pg_get_constraintdef`/`pg_get_indexdef` (`core/pg_catalog.rs:4661-4788`) do the same **per row**, making joins (e.g. psql's `\d tablename`) O(T²). Fixing this needs either per-`Connection` OID-map caching with schema-version invalidation, or restructuring the `pg_get_*` functions to accept a pre-built map — an architectural change spanning multiple cursors, not a bite-sized patch. **No code fix is given here; this must be scheduled as its own design spike**, not squeezed into this plan's task format.

---

### Task E-medium-indexdef-arity: Fix pg_get_indexdef arity mismatch

**Files:** Modify: `core/function.rs:1046` | Cross-file: `core/vdbe/execute.rs:7176-7182` | Test: `tests/integration/postgres/catalog.rs`

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_pg_get_indexdef_accepts_three_arg_form(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare(
        "SELECT pg_get_indexdef(indexrelid, 0, true) FROM pg_index idx
         JOIN pg_class c ON c.oid = idx.indrelid WHERE c.relname = 't' AND idx.indisunique = 1",
    ).unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => {}
        _ => panic!("pg_get_indexdef(oid, 0, true) failed to execute"),
    }
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_get_indexdef_accepts_three_arg_form` — Expected: FAIL, arity-validation error rejects the 3-arg call.

- [ ] **Step 3: Implement**

```rust
// core/function.rs:1046 — before
Self::PgFormatType | Self::PgGetConstraintDef | Self::PgGetIndexDef => &[1, 2],
```
```rust
// after
Self::PgFormatType | Self::PgGetConstraintDef => &[1, 2],
Self::PgGetIndexDef => &[1, 3],
```
```rust
// core/vdbe/execute.rs:7176-7182 — before
ScalarFunc::PgGetIndexDef => {
    let oid = state.registers[*start_reg].get_value().as_int().unwrap_or(0);
    state.registers[*dest].set_value(exec_pg_get_indexdef(&program.connection, oid));
}
```
```rust
// after
ScalarFunc::PgGetIndexDef => {
    let oid = state.registers[*start_reg].get_value().as_int().unwrap_or(0);
    // column_no/pretty (args 2,3) accepted for real-PG arity compatibility;
    // per-column definitions are not implemented — full index def is always
    // returned, matching column_no=0 semantics.
    state.registers[*dest].set_value(exec_pg_get_indexdef(&program.connection, oid));
}
```

- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): accept 3-arg form of pg_get_indexdef matching real PG arity"`

---

### Task E-medium-trigger: Add tgparentid to pg_trigger stub

**Files:** Modify: `core/pg_catalog.rs:3658`

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_pg_trigger_has_tgparentid_column(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.prepare("SELECT tgparentid FROM pg_trigger").unwrap(); // must not error
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_trigger_has_tgparentid_column` — Expected: FAIL, "no such column: tgparentid".

- [ ] **Step 3: Implement** — insert `tgparentid INTEGER` after `tgrelid` in the `empty_catalog_table("pg_trigger", "CREATE TABLE pg_trigger (oid INTEGER, tgrelid INTEGER, ...)")` DDL string, matching real PostgreSQL's column order.

- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): add tgparentid to pg_trigger stub for PG13+ psql compat"`

---

### Task E-medium-enum-ns: Fix user enum typnamespace hardcoded to pg_catalog

**Files:** Modify: `core/pg_catalog.rs:2789-2831` (`PgTypeCursor::load_types`, user-enum row builder)

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_pg_type_user_enum_in_public_namespace(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    conn.execute("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')").unwrap();
    let mut stmt = conn.prepare("SELECT typnamespace FROM pg_type WHERE typname = 'mood'").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => assert_eq!(stmt.row().unwrap().get_value(0).as_int(), Some(2200), "user enum should live in public (2200)"),
        _ => panic!("mood enum not found"),
    }
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_type_user_enum_in_public_namespace` — Expected: FAIL, `typnamespace == 11`.
- [ ] **Step 3: Implement** — replace `Value::from_i64(11), // typnamespace (pg_catalog)` with `Value::from_i64(2200), // typnamespace (public) — user-defined types live in the schema they were created in, not pg_catalog (reserved for built-ins).`
- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): user-defined enums live in public namespace, not pg_catalog"`

Revisit once the H26 spike lands to use the enum's actual owning schema OID instead of hardcoded `2200`.

---

### Task E-medium-money: Fix pg_type money row typbyval

**Files:** Modify: `core/pg_catalog.rs:2586-2597`

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_pg_type_money_is_byval(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare("SELECT typbyval FROM pg_type WHERE typname = 'money'").unwrap();
    match stmt.step().unwrap() {
        StepResult::Row => assert_eq!(stmt.row().unwrap().get_value(0).as_int(), Some(1), "money is int8-backed, must be pass-by-value"),
        _ => panic!("money type not found"),
    }
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_type_money_is_byval` — Expected: FAIL.
- [ ] **Step 3: Implement** — in the `money` `PgTypeInfo` literal, change `typbyval: false` to `typbyval: true`.
- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): money type is pass-by-value (int8-backed)"`

Unblocks Task E7 (H28) from inheriting this bug for `money`-typed columns — land before or together with E7.

---

### Task E-medium-check-name: Disambiguate unnamed CHECK constraint names

**Files:** Modify: `core/pg_catalog.rs:3321-3326` (`PgConstraintCursor::load_constraints`, CHECK block)

- [ ] **Step 1: Write failing test**

```rust
#[turso_macros::test]
fn test_pg_constraint_unnamed_checks_get_distinct_conname(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (a INTEGER CHECK (a > 0), b INTEGER CHECK (b > 0))").unwrap();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();
    let mut stmt = conn.prepare(
        "SELECT conname FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid WHERE c.relname = 't' AND con.contype = 'c'",
    ).unwrap();
    let mut names = std::collections::HashSet::new();
    loop {
        match stmt.step().unwrap() {
            StepResult::Row => { let Value::Text(n) = stmt.row().unwrap().get_value(0) else { panic!("expected text") }; names.insert(n.as_str().to_string()); }
            StepResult::Done => break,
            _ => {}
        }
    }
    assert_eq!(names.len(), 2, "two unnamed CHECK constraints must not collide on conname, got {names:?}");
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_pg_constraint_unnamed_checks_get_distinct_conname` — Expected: FAIL, `names.len() == 1`.

- [ ] **Step 3: Implement**

```rust
// before
for chk in &btree.check_constraints {
    let conname = chk.name.clone().unwrap_or_else(|| format!("{table_name}_check"));
}
```
```rust
// after
for (chk_idx, chk) in btree.check_constraints.iter().enumerate() {
    let conname = chk.name.clone().unwrap_or_else(|| {
        if chk_idx == 0 { format!("{table_name}_check") } else { format!("{table_name}_check{}", chk_idx + 1) }
    });
}
```

- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(pg_catalog): disambiguate unnamed CHECK constraint names on the same table"`

---

### Task E-low-unnamed-col (judgment call — confirm with coordinator before implementing): Unnamed column silently becomes attname=""

**Files:** Modify: `core/pg_catalog.rs:786`

`Column.name: Option<String>` can legitimately be `None` for expression-derived/anonymous columns — this may not be a bug. Recommended: log a warning rather than hard-fail.

```rust
// after
let col_name = col.name.clone().unwrap_or_else(|| {
    tracing::warn!(table = %table_name, position = i, "column has no name; emitting empty attname");
    String::new()
});
```

- [ ] **Step 1: Commit if approved** — `git commit -S -m "fix(pg_catalog): log warning when emitting empty attname for unnamed column"`

---

### Task E-low-encoding: pg_encoding_to_char() maps unknown encodings to UTF8

**Files:** Modify: `core/functions/postgres.rs:41-48` | Test: inline `mod tests`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn pg_encoding_to_char_unknown_encoding_errors() {
    // real PostgreSQL returns "" for an out-of-range encoding id, never errors.
    assert_eq!(exec_pg_encoding_to_char(9999), Value::build_text(""));
}
```

- [ ] **Step 2: Verify fail** — Run: `cargo test -p turso_core --lib functions::postgres::tests::pg_encoding_to_char_unknown_encoding_errors` — Expected: FAIL, returns `"UTF8"`.
- [ ] **Step 3: Implement** — change the `_ =>` arm from `"UTF8"` to `""`.
- [ ] **Step 4: Verify pass** — same command, Expected: PASS.
- [ ] **Step 5: Commit** — `git commit -S -m "fix(functions): pg_encoding_to_char returns empty string for unknown encodings"`

---

### Task E-low-role-quote: `CREATE ROLE "Alice"` must preserve case instead of folding to lowercase

**Files:** Modify: `core/pg_role.rs:206-229` (`normalize_role_name`) | Test: `tests/integration/postgres/catalog.rs`

**Root cause (verified by reading the code, not just the finding text):** `normalize_role_name` already detects and strips surrounding `"`/`'` quotes (lines 208-214) — the quoted-vs-unquoted distinction is computed — but then throws it away at line 228 by unconditionally calling `.to_lowercase()` regardless of whether the name was quoted. This is a self-contained, one-function bug, unlike H34b's deeper `normalize_ident()` issue (which is shared across the whole identifier-resolution path) — no two-plan-rule follow-up needed here.

- [ ] **Step 1: Write the failing test**

```rust
// tests/integration/postgres/catalog.rs

#[turso_macros::test(mvcc)]
fn test_postgres_quoted_role_name_preserves_case(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE ROLE \"Alice\"").unwrap();

    let mut rows = conn
        .query("SELECT rolname FROM pg_roles WHERE rolname = 'Alice'")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("CREATE ROLE \"Alice\" should preserve case, but no rolname='Alice' row was found");
    };
}

#[turso_macros::test(mvcc)]
fn test_postgres_unquoted_role_name_still_folds_lowercase(db: TempDatabase) {
    // Regression guard: unquoted identifiers must keep PG's normal fold-to-lowercase behavior.
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE ROLE Bob").unwrap();

    let mut rows = conn
        .query("SELECT rolname FROM pg_roles WHERE rolname = 'bob'")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("unquoted CREATE ROLE Bob should fold to lowercase 'bob'");
    };
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_postgres_quoted_role_name_preserves_case -- --nocapture`
Expected: FAIL — `rolname` is `'alice'` (lowercased) even though the name was quoted, so no row matches `rolname = 'Alice'`.

- [ ] **Step 3: Implement**

```rust
// core/pg_role.rs — before (lines 206-229)
fn normalize_role_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    let unquoted = if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    if unquoted.is_empty() {
        return Err(LimboError::ParseError(
            "role name cannot be empty".to_string(),
        ));
    }
    if !unquoted
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(LimboError::ParseError(format!(
            "invalid role name \"{unquoted}\""
        )));
    }
    Ok(unquoted.to_lowercase())
}
```

```rust
// core/pg_role.rs — after
fn normalize_role_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    let is_quoted = (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''));
    let unquoted = if is_quoted {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    if unquoted.is_empty() {
        return Err(LimboError::ParseError(
            "role name cannot be empty".to_string(),
        ));
    }
    if !unquoted
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(LimboError::ParseError(format!(
            "invalid role name \"{unquoted}\""
        )));
    }
    // Matches PostgreSQL identifier-folding: quoted names keep their case,
    // unquoted names fold to lowercase.
    if is_quoted {
        Ok(unquoted.to_string())
    } else {
        Ok(unquoted.to_lowercase())
    }
}
```

`role_by_name` (lines 131-136) already does a case-insensitive `eq_ignore_ascii_case` lookup, so `DROP ROLE "Alice"` / `DROP ROLE alice` both still resolve to the stored `"Alice"` row without any change there — only the *stored* casing changes, not the lookup semantics.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::catalog::test_postgres_quoted_role_name_preserves_case integration::postgres::catalog::test_postgres_unquoted_role_name_still_folds_lowercase -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/pg_role.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(pg_role): CREATE ROLE preserves case for quoted identifiers instead of always folding lowercase"
```

**Notes:** Independent of every other Workstream E task — different file (`core/pg_role.rs`, not currently listed in the Workstream Map's "Owns (files)" column for E; this task adds it). No interaction with `pg_roles`/`pg_authid`/`pg_user` catalog row-generation code (`pg_role_registry_rows`), which just reads back whatever `name` was stored.

---

### Workstream E sequencing notes (from catalog.md's own coordinator notes)

1. C1/C2/C3 are fully independent — parallelize immediately, highest severity (panic/DoS/crash fixes).
2. H25 (E4) and H31 (E10) share the column-position-set-matching fix technique — assign to the same engineer, land as 2 separate commits (different cursors).
3. H24 (E5) needs the broader OID-redesign acknowledgment (combine with E-medium-oid-const) before it can be scheduled as a real task — do not attempt piecemeal.
4. H26 (E-attached-schema) needs a `core/lib.rs` API investigation spike first — not schedulable as a bite-sized task until that spike defines the accessor shape.
5. H32 (E11) and the DDL trailing-space Low finding touch the identical function (`convert_sqlite_ddl_to_postgres`) — one PR, two logical fixes, already bundled into Task E11 above.
6. The O(N²) full-schema-rescan Medium finding (E-medium-perf) is a design spike, not a fix ticket — do not force it into a bite-sized task shape.

---

## Workstream B — Translator

**Worktree:** `wt-translator` · **Primary file:** `parser_pg/src/translator.rs` · **Test command:** `cargo test -p turso_parser_pg`

Landing order (per translator.md's own coordinator notes): H1 → H2 → H9 (same loop, build in that sequence) → H13 → H11 → M1 → H4 → M2 → M3 → C7 → H6 → H7 → H10 → H3 (covers H12 too) → H20 (coordinate with Workstream C) → H5 / H8 (interim-reject only) → remaining Mediums as capacity allows.

### Task B1: Preserve column-level CHECK constraints (H1)

**Files:**
- Modify: `parser_pg/src/translator.rs:331-350` (constraint loop in `translate_create_table_column`)

**Interfaces:**
- Consumes: `ast::ColumnConstraint::Check(Box<Expr>)` (already exists, `parser/src/ast.rs:1499`)
- Produces: `check_exprs: Vec<ast::Expr>` local variable that Task B2 and Task B3 (H9) also extend — land B1 before B2/B3, or combine into one commit.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_column_check_constraint_preserved() {
    let translator = PostgreSQLTranslator::new();
    let sql = "CREATE TABLE t (age INTEGER CHECK (age >= 0))";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::CreateTable {
        body: ast::CreateTableBody::ColumnsAndConstraints { columns, .. },
        ..
    } = translated
    {
        let col = &columns[0];
        let has_check = col
            .constraints
            .iter()
            .any(|c| matches!(c.constraint, ast::ColumnConstraint::Check(_)));
        assert!(has_check, "CHECK constraint was dropped: {:?}", col.constraints);
    } else {
        panic!("Expected CreateTable");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_column_check_constraint_preserved -- --nocapture`
Expected: FAIL (`has_check` is `false` — CHECK constraint silently dropped).

- [ ] **Step 3: Implement**

```rust
// before (translate_create_table_column, lines 331-350)
for constraint_node in &col_def.constraints {
    let Some(Node::Constraint(constraint)) = &constraint_node.node else {
        continue;
    };
    let contype = ConstrType::try_from(constraint.contype).unwrap_or(ConstrType::Undefined);
    match contype {
        ConstrType::ConstrPrimary => is_primary_key = true,
        ConstrType::ConstrNotnull => is_not_null = true,
        ConstrType::ConstrUnique => is_unique = true,
        ConstrType::ConstrDefault => {
            if let Some(ref raw_expr) = constraint.raw_expr {
                default_expr = Some(self.translate_expr(raw_expr)?);
            }
        }
        ConstrType::ConstrForeign => {
            foreign_key = extract_foreign_key(constraint);
        }
        _ => {}
    }
}

// after
let mut check_exprs: Vec<ast::Expr> = Vec::new();
for constraint_node in &col_def.constraints {
    let Some(Node::Constraint(constraint)) = &constraint_node.node else {
        continue;
    };
    let contype = ConstrType::try_from(constraint.contype).unwrap_or(ConstrType::Undefined);
    match contype {
        ConstrType::ConstrPrimary => is_primary_key = true,
        ConstrType::ConstrNotnull => is_not_null = true,
        ConstrType::ConstrUnique => is_unique = true,
        ConstrType::ConstrDefault => {
            if let Some(ref raw_expr) = constraint.raw_expr {
                default_expr = Some(self.translate_expr(raw_expr)?);
            }
        }
        ConstrType::ConstrForeign => {
            foreign_key = extract_foreign_key(constraint);
        }
        ConstrType::ConstrCheck => {
            if let Some(ref raw_expr) = constraint.raw_expr {
                check_exprs.push(self.translate_expr(raw_expr)?);
            }
        }
        _ => {}
    }
}
```

After the existing FK-emitting block (currently the last block before `Ok(ast::ColumnDefinition {...})`, lines 415-426), add:

```rust
for expr in check_exprs {
    constraints.push(ast::NamedColumnConstraint {
        name: None,
        constraint: ast::ColumnConstraint::Check(Box::new(expr)),
    });
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_column_check_constraint_preserved -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): preserve column-level CHECK constraints in CREATE TABLE"
```

---

### Task B2: Preserve GENERATED ... STORED columns; reject GENERATED ... AS IDENTITY (H2)

**Files:**
- Modify: `parser_pg/src/translator.rs:331-350` (same loop as B1, `translate_create_table_column`)

**Interfaces:**
- Consumes: `check_exprs` loop scaffold from Task B1 (must land first). `ast::ColumnConstraint::Generated { expr: Box<Expr>, typ: Option<GeneratedColumnType> }` (`parser/src/ast.rs:1514-1531`, already exists).
- Produces: `generated_expr: Option<ast::Expr>` local variable, consumed by Task B3 (H9)'s shared helper.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_generated_column_preserved() {
    let translator = PostgreSQLTranslator::new();
    let sql = "CREATE TABLE t (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED)";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::CreateTable {
        body: ast::CreateTableBody::ColumnsAndConstraints { columns, .. },
        ..
    } = translated
    {
        let b = &columns[1];
        let generated = b.constraints.iter().find_map(|c| match &c.constraint {
            ast::ColumnConstraint::Generated { expr: _, typ } => Some(*typ),
            _ => None,
        });
        assert_eq!(
            generated,
            Some(Some(ast::GeneratedColumnType::Stored)),
            "GENERATED ALWAYS AS (...) STORED was dropped: {:?}",
            b.constraints
        );
    } else {
        panic!("Expected CreateTable");
    }
}

#[test]
fn test_identity_column_rejected_not_silently_dropped() {
    let translator = PostgreSQLTranslator::new();
    let sql = "CREATE TABLE t (id INTEGER GENERATED ALWAYS AS IDENTITY)";
    let parsed = crate::parse(sql).unwrap();
    let err = translator.translate(&parsed).unwrap_err();
    assert!(matches!(err, ParseError::ParseError(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_parser_pg test_generated_column_preserved test_identity_column_rejected_not_silently_dropped -- --nocapture`
Expected: FAIL — `generated` is `None` for the first test; the second gets `Ok(..)` instead of an error (IDENTITY silently dropped).

- [ ] **Step 3: Implement**

Add to the same match in `translate_create_table_column` (after B1's `ConstrCheck` arm):

```rust
ConstrType::ConstrGenerated => {
    if let Some(ref raw_expr) = constraint.raw_expr {
        generated_expr = Some(self.translate_expr(raw_expr)?);
    }
}
ConstrType::ConstrIdentity => {
    return Err(ParseError::ParseError(
        "GENERATED ... AS IDENTITY is not supported (no Turso equivalent to PG sequences); \
         use SERIAL or a manually managed default instead".into(),
    ));
}
```

Declare `let mut generated_expr: Option<ast::Expr> = None;` alongside `check_exprs`, and after the CHECK-constraint emission block from B1 add:

```rust
if let Some(expr) = generated_expr {
    constraints.push(ast::NamedColumnConstraint {
        name: None,
        constraint: ast::ColumnConstraint::Generated {
            expr: Box::new(expr),
            typ: Some(ast::GeneratedColumnType::Stored),
        },
    });
}
```

PG's `GENERATED ... AS (expr)` is always `STORED` (PG has no virtual generated columns), so hardcoding `typ: Some(Stored)` is correct.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_parser_pg test_generated_column_preserved test_identity_column_rejected_not_silently_dropped -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): preserve GENERATED ... STORED columns, reject GENERATED ... AS IDENTITY"
```

**Two-plan note:** real `GENERATED ... AS IDENTITY` support needs (1) a Turso-core plan for a generic column-level identity/sequence feature with no mention of Postgres, then (2) a pgmicro plan wiring `ConstrIdentity` to it. Not scheduled here.

---

### Task B3: Bring ALTER TABLE ADD COLUMN to parity via a shared constraint-translation helper (H9)

**Files:**
- Modify: `parser_pg/src/translator.rs:675-743` (`translate_column_def`, ALTER TABLE path)
- Modify: `parser_pg/src/translator.rs:306-432` (`translate_create_table_column`, refactored to use the new helper)

**Interfaces:**
- Consumes: Tasks B1 and B2 must have landed first — the CREATE TABLE loop this task extracts from already contains `check_exprs`/`generated_expr`.
- Produces: `struct PgColumnConstraintParts` and `fn translate_column_constraints(&self, constraints: &[pg_query::protobuf::Node]) -> Result<PgColumnConstraintParts, ParseError>` — new shared entry point other translator code may reuse for future constraint-handling call sites (e.g. `ALTER COLUMN TYPE`, if added later).

**Root cause (confirmed duplication):** `translate_column_def` (ALTER TABLE path, lines 706-729) handles only `ConstrPrimary`/`ConstrNotnull`/`ConstrUnique`/`ConstrDefault` — no FK, no CHECK, no GENERATED/IDENTITY. `translate_create_table_column` (CREATE TABLE path) handles FK and (after B1/B2) CHECK/GENERATED/IDENTITY. Two independently-maintained loops over the same `ColumnDef.constraints` list, each missing a different subset.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_alter_table_add_column_preserves_fk_and_check() {
    let translator = PostgreSQLTranslator::new();
    let sql = "ALTER TABLE orders ADD COLUMN cust_id INTEGER \
               REFERENCES customers(id) CHECK (cust_id > 0)";
    let parsed = crate::parse(sql).unwrap();
    let stmts = translator.translate_stmts(&parsed).unwrap();
    assert_eq!(stmts.len(), 1);

    if let ast::Stmt::AlterTable(ast::AlterTable {
        body: ast::AlterTableBody::AddColumn(col),
        ..
    }) = &stmts[0]
    {
        let has_fk = col
            .constraints
            .iter()
            .any(|c| matches!(c.constraint, ast::ColumnConstraint::ForeignKey { .. }));
        let has_check = col
            .constraints
            .iter()
            .any(|c| matches!(c.constraint, ast::ColumnConstraint::Check(_)));
        assert!(has_fk, "FK dropped by ADD COLUMN: {:?}", col.constraints);
        assert!(has_check, "CHECK dropped by ADD COLUMN: {:?}", col.constraints);
    } else {
        panic!("Expected AlterTable::AddColumn");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_alter_table_add_column_preserves_fk_and_check -- --nocapture`
Expected: FAIL (`has_fk` is `false`).

- [ ] **Step 3: Implement**

New shared struct + helper (add near `translate_create_table_column`, replaces both loops):

```rust
/// Constraint fragments parsed from a PG `ColumnDef.constraints` list, shared by
/// the CREATE TABLE and ALTER TABLE ADD COLUMN paths. Table-level concerns (whether
/// PK implies AUTOINCREMENT, whether a table-level PK suppresses the column-level
/// one) stay in the CREATE-TABLE-only caller; this only does per-constraint
/// translation, so both paths see the exact same set of supported constraint kinds.
#[derive(Default)]
struct PgColumnConstraintParts {
    is_primary_key: bool,
    is_not_null: bool,
    is_unique: bool,
    default_expr: Option<ast::Expr>,
    foreign_key: Option<PgForeignKey>,
    check_exprs: Vec<ast::Expr>,
    generated_expr: Option<ast::Expr>,
}

impl PostgreSQLTranslator {
    fn translate_column_constraints(
        &self,
        constraints: &[pg_query::protobuf::Node],
    ) -> Result<PgColumnConstraintParts, ParseError> {
        use pg_query::protobuf::node::Node;
        use pg_query::protobuf::ConstrType;

        let mut parts = PgColumnConstraintParts::default();
        for constraint_node in constraints {
            let Some(Node::Constraint(constraint)) = &constraint_node.node else {
                continue;
            };
            let contype = ConstrType::try_from(constraint.contype).unwrap_or(ConstrType::Undefined);
            match contype {
                ConstrType::ConstrPrimary => parts.is_primary_key = true,
                ConstrType::ConstrNotnull => parts.is_not_null = true,
                ConstrType::ConstrUnique => parts.is_unique = true,
                ConstrType::ConstrDefault => {
                    if let Some(ref raw_expr) = constraint.raw_expr {
                        parts.default_expr = Some(self.translate_expr(raw_expr)?);
                    }
                }
                ConstrType::ConstrForeign => {
                    parts.foreign_key = extract_foreign_key(constraint);
                }
                ConstrType::ConstrCheck => {
                    if let Some(ref raw_expr) = constraint.raw_expr {
                        parts.check_exprs.push(self.translate_expr(raw_expr)?);
                    }
                }
                ConstrType::ConstrGenerated => {
                    if let Some(ref raw_expr) = constraint.raw_expr {
                        parts.generated_expr = Some(self.translate_expr(raw_expr)?);
                    }
                }
                ConstrType::ConstrIdentity => {
                    return Err(ParseError::ParseError(
                        "GENERATED ... AS IDENTITY is not supported".into(),
                    ));
                }
                _ => {}
            }
        }
        Ok(parts)
    }
}
```

`translate_column_def` (ALTER TABLE path) becomes:

```rust
fn translate_column_def(
    &self,
    col_def: &pg_query::protobuf::ColumnDef,
) -> Result<ast::ColumnDefinition, ParseError> {
    let col_name = ast::Name::from_string(&col_def.colname);
    let pg_type = extract_type_name(col_def)?;
    let typmods = extract_integer_typmods(col_def);
    let mapping = map_pg_type(&pg_type, &typmods).ok_or_else(|| {
        ParseError::ParseError(format!("unsupported PostgreSQL type: {pg_type}"))
    })?;
    let size = match mapping.type_params.as_slice() {
        [p, s] => Some(ast::TypeSize::TypeSize(
            Box::new(ast::Expr::Literal(ast::Literal::Numeric(p.to_string()))),
            Box::new(ast::Expr::Literal(ast::Literal::Numeric(s.to_string()))),
        )),
        [n] => Some(ast::TypeSize::MaxSize(Box::new(ast::Expr::Literal(
            ast::Literal::Numeric(n.to_string()),
        )))),
        _ => None,
    };
    let col_type = Some(ast::Type {
        name: mapping.type_name,
        size,
        array_dimensions: mapping.array_dimensions,
    });

    let parts = self.translate_column_constraints(&col_def.constraints)?;
    let mut constraints = Vec::new();
    if parts.is_primary_key {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::PrimaryKey {
                order: None,
                conflict_clause: None,
                auto_increment: false,
            },
        });
    }
    if parts.is_not_null && !parts.is_primary_key {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::NotNull { nullable: false, conflict_clause: None },
        });
    }
    if parts.is_unique {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::Unique(None),
        });
    }
    if let Some(expr) = parts.default_expr {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::Default(Box::new(expr)),
        });
    }
    if let Some(fk) = &parts.foreign_key {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::ForeignKey {
                clause: self.pg_fk_to_fk_clause(fk),
                defer_clause: None,
            },
        });
    }
    for expr in parts.check_exprs {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::Check(Box::new(expr)),
        });
    }
    if let Some(expr) = parts.generated_expr {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::Generated {
                expr: Box::new(expr),
                typ: Some(ast::GeneratedColumnType::Stored),
            },
        });
    }

    Ok(ast::ColumnDefinition { col_name, col_type, constraints })
}
```

`translate_create_table_column` (lines 306-432) is refactored the same way: replace its inline loop (post-B1/B2) with `let parts = self.translate_column_constraints(&col_def.constraints)?;`, then keep its CREATE-TABLE-specific logic (`is_serial`, `has_table_pk`/`has_autoincrement` suppression) layered on top of `parts.is_primary_key`/`parts.default_expr`/etc.

- [ ] **Step 4: Run test to verify it passes, plus full regression**

Run: `cargo test -p turso_parser_pg test_alter_table_add_column_preserves_fk_and_check test_column_check_constraint_preserved test_generated_column_preserved -- --nocapture`
Expected: PASS (all three — confirms the refactor didn't regress B1/B2's CREATE TABLE behavior).

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): extract shared column-constraint translation, fix ALTER TABLE ADD COLUMN dropping FK/CHECK/GENERATED"
```

---

### Task B4: Route `information_schema.tables`/`.columns` through a schema-aware mapping (H13)

**Files:**
- Modify: `parser_pg/src/translator.rs:46-108` (`qualified_name_from_range_var`, `map_table_name`)

**Interfaces:**
- Produces: `fn map_information_schema_table(table_name: &str) -> String` (new associated function on `PostgreSQLTranslator`).

**Root cause (confirmed dead code):** `map_table_name` has match arms for the literal strings `"information_schema.tables"`/`"information_schema.columns"`, but its only caller (`qualified_name_from_range_var`) always passes just `range_var.relname` — the unqualified name (`"tables"`, `"columns"`), never the dotted form, since PG's parser already splits `schemaname`/`relname`. Separately, `qualified_name_from_range_var` special-cases `schemaname == "information_schema"` to fall through to a bare, unmapped name — so today `SELECT * FROM information_schema.tables` resolves to a nonexistent table literally named `tables`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_information_schema_tables_maps_to_sqlite_master() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT * FROM information_schema.tables";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { from, .. } = &select.body.select {
            let from_clause = from.as_ref().expect("Expected FROM clause");
            if let ast::SelectTable::Table(qualified_name, _, _) = &*from_clause.select {
                assert_eq!(qualified_name.name.as_str(), "sqlite_master");
            } else {
                panic!("Expected table reference");
            }
        }
    } else {
        panic!("Expected Select");
    }
}

#[test]
fn test_information_schema_columns_maps_to_pragma_table_info() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT * FROM information_schema.columns";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { from, .. } = &select.body.select {
            let from_clause = from.as_ref().expect("Expected FROM clause");
            if let ast::SelectTable::Table(qualified_name, _, _) = &*from_clause.select {
                assert_eq!(qualified_name.name.as_str(), "pragma_table_info");
            } else {
                panic!("Expected table reference");
            }
        }
    } else {
        panic!("Expected Select");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_parser_pg test_information_schema_tables_maps_to_sqlite_master test_information_schema_columns_maps_to_pragma_table_info -- --nocapture`
Expected: FAIL — `qualified_name.name` is `"tables"`/`"columns"`, not the mapped names.

- [ ] **Step 3: Implement**

```rust
// before (qualified_name_from_range_var, lines 46-69)
fn qualified_name_from_range_var(
    &self,
    range_var: &pg_query::protobuf::RangeVar,
) -> ast::QualifiedName {
    let mapped_name = self.map_table_name(&range_var.relname);
    let name = ast::Name::from_string(mapped_name);
    let alias = range_var
        .alias
        .as_ref()
        .filter(|a| !a.aliasname.is_empty())
        .map(|a| ast::Name::from_string(&a.aliasname));
    let mut qn = if range_var.schemaname.is_empty()
        || matches!(
            range_var.schemaname.to_lowercase().as_str(),
            "pg_catalog" | "public" | "information_schema"
        ) {
        ast::QualifiedName::single(name)
    } else {
        let schema = ast::Name::from_string(range_var.schemaname.clone());
        ast::QualifiedName::fullname(schema, name)
    };
    qn.alias = alias;
    qn
}

// after
fn qualified_name_from_range_var(
    &self,
    range_var: &pg_query::protobuf::RangeVar,
) -> ast::QualifiedName {
    let schema_lower = range_var.schemaname.to_lowercase();
    let mapped_name = if schema_lower == "information_schema" {
        Self::map_information_schema_table(&range_var.relname)
    } else {
        self.map_table_name(&range_var.relname)
    };
    let name = ast::Name::from_string(mapped_name);
    let alias = range_var
        .alias
        .as_ref()
        .filter(|a| !a.aliasname.is_empty())
        .map(|a| ast::Name::from_string(&a.aliasname));
    let mut qn = if range_var.schemaname.is_empty()
        || matches!(schema_lower.as_str(), "pg_catalog" | "public" | "information_schema")
    {
        ast::QualifiedName::single(name)
    } else {
        let schema = ast::Name::from_string(range_var.schemaname.clone());
        ast::QualifiedName::fullname(schema, name)
    };
    qn.alias = alias;
    qn
}

/// Maps `information_schema.<table>` names to their SQLite equivalents.
fn map_information_schema_table(table_name: &str) -> String {
    match table_name.to_lowercase().as_str() {
        "tables" => "sqlite_master".to_string(),
        "columns" => "pragma_table_info".to_string(),
        _ => table_name.to_string(),
    }
}
```

Remove the now-truly-dead arms from `map_table_name`:

```rust
// before (lines 102-104)
| "pg_tables" => table_name.to_string(),
"information_schema.tables" => "sqlite_master".to_string(),
"information_schema.columns" => "pragma_table_info".to_string(),
// Default: keep original name
_ => table_name.to_string(),

// after
| "pg_tables" => table_name.to_string(),
// Default: keep original name
_ => table_name.to_string(),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_parser_pg test_information_schema_tables_maps_to_sqlite_master test_information_schema_columns_maps_to_pragma_table_info -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): route schema-qualified information_schema.tables/.columns through mapping"
```

**Notes:** `pragma_table_info` needs a table-name argument in real usage (`SELECT * FROM pragma_table_info('t')`); `information_schema.columns` without a `WHERE table_name = ...` filter won't list columns for all tables like PG does. That's a deeper gap beyond "dead code" — flagged as a separate follow-up finding, out of scope here.

---

### Task B5: Fix `LIKE`/`ILIKE ... ESCAPE` mistranslation to a nonexistent `like_escape` function (H11)

**Files:**
- Modify: `parser_pg/src/translator.rs:3055-3182` (`translate_like_expr`, `translate_ilike_expr`)

**Interfaces:**
- Produces: `fn translate_like_pattern_and_escape(&self, rexpr: &pg_query::protobuf::Node) -> Result<(ast::Expr, Option<Box<ast::Expr>>), ParseError>`.
- Consumes: `ast::Expr::Like.escape: Option<Box<Expr>>` (already exists, `parser/src/ast.rs:581-582`).

**Root cause:** `pg_query` represents `X LIKE Y ESCAPE Z` by wrapping the pattern in a synthetic call `like_escape(Y, Z)` (same trick used for `SIMILAR TO` → `similar_to_escape`, already correctly unwrapped at `translator.rs:2718-2731`). `translate_like_expr`/`translate_ilike_expr` blindly `translate_expr` the wrapped rhs, which falls through to the generic `FuncCall` path and emits a call to a SQL function literally named `like_escape` — which does not exist in Turso, so the query fails at execution with "no such function: like_escape".

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_like_escape_clause_preserved() {
    let translator = PostgreSQLTranslator::new();
    let sql = r"SELECT * FROM users WHERE name LIKE '50\%%' ESCAPE '\'";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { where_clause, .. } = &select.body.select {
            if let ast::Expr::Like { rhs, escape, .. } = &**where_clause.as_ref().unwrap() {
                assert!(escape.is_some(), "ESCAPE clause was dropped");
                assert!(
                    matches!(**rhs, ast::Expr::Literal(ast::Literal::String(_))),
                    "rhs should be the unwrapped pattern literal, not a like_escape(...) call: {rhs:?}"
                );
            } else {
                panic!("Expected Like expression");
            }
        }
    } else {
        panic!("Expected Select");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_like_escape_clause_preserved -- --nocapture`
Expected: FAIL (`escape` is `None`, `rhs` is a `FunctionCall` to `like_escape`).

- [ ] **Step 3: Implement**

```rust
// before (translate_like_expr, lines 3095-3109)
let rhs = if let Some(rexpr) = &a_expr.rexpr {
    Box::new(self.translate_expr(rexpr)?)
} else {
    return Err(ParseError::ParseError(
        "Missing right expression for LIKE operator".to_string(),
    ));
};

Ok(ast::Expr::Like {
    lhs,
    not,
    op: ast::LikeOperator::Like,
    rhs,
    escape: None,
})

// after
let rexpr = a_expr
    .rexpr
    .as_ref()
    .ok_or_else(|| ParseError::ParseError("Missing right expression for LIKE operator".to_string()))?;
let (rhs, escape) = self.translate_like_pattern_and_escape(rexpr)?;

Ok(ast::Expr::Like {
    lhs,
    not,
    op: ast::LikeOperator::Like,
    rhs: Box::new(rhs),
    escape,
})
```

Apply the identical change at `translate_ilike_expr`'s `rhs` extraction (lines 3144-3150), feeding into the existing `lower_rhs` wrapping. New shared helper (place near `translate_like_expr`):

```rust
/// PG wraps `X LIKE Y ESCAPE Z` as `like_escape(Y, Z)`; unwrap it so `Y` becomes the
/// pattern and `Z` becomes the AST `escape` field, instead of falling through to a
/// generic FuncCall to a nonexistent `like_escape` SQL function.
fn translate_like_pattern_and_escape(
    &self,
    rexpr: &pg_query::protobuf::Node,
) -> Result<(ast::Expr, Option<Box<ast::Expr>>), ParseError> {
    if let Some(pg_query::protobuf::node::Node::FuncCall(fc)) = &rexpr.node {
        let is_like_escape = fc
            .funcname
            .iter()
            .filter_map(|n| match &n.node {
                Some(pg_query::protobuf::node::Node::String(s)) => Some(s.sval.as_str()),
                _ => None,
            })
            .next_back()
            == Some("like_escape");
        if is_like_escape {
            let pattern = fc
                .args
                .first()
                .ok_or_else(|| ParseError::ParseError("like_escape: missing pattern".into()))?;
            let pattern_expr = self.translate_expr(pattern)?;
            let escape_expr = match fc.args.get(1) {
                Some(esc) => Some(Box::new(self.translate_expr(esc)?)),
                None => None,
            };
            return Ok((pattern_expr, escape_expr));
        }
    }
    Ok((self.translate_expr(rexpr)?, None))
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_like_escape_clause_preserved -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): stop mistranslating LIKE/ILIKE ESCAPE to a nonexistent like_escape() call"
```

**Notes:** Before considering this fully closed, add `test_ilike_escape_clause_preserved` mirroring the LIKE test, and verify PG's actual behavior for whether the escape character itself needs `LOWER()`-wrapping under `ILIKE` (it shouldn't — escape chars aren't typically alphabetic — but confirm against real PostgreSQL, per the "translator correctness over coverage" rule).

---

### Task B6: Boolean literals use `Literal::True`/`Literal::False`, not `Literal::Numeric("0"/"1")` (M1)

**Files:**
- Modify: `parser_pg/src/translator.rs:2605-2610` (`translate_const`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_boolean_literal_uses_true_false_variant() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT true, false";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();
    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { columns, .. } = &select.body.select {
            assert!(matches!(
                columns[0],
                ast::ResultColumn::Expr(ref e, _) if matches!(**e, ast::Expr::Literal(ast::Literal::True))
            ));
            assert!(matches!(
                columns[1],
                ast::ResultColumn::Expr(ref e, _) if matches!(**e, ast::Expr::Literal(ast::Literal::False))
            ));
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_boolean_literal_uses_true_false_variant -- --nocapture`
Expected: FAIL (literals are `Numeric("1")`/`Numeric("0")`, not `True`/`False`).

- [ ] **Step 3: Implement**

```rust
// before
pg_query::protobuf::a_const::Val::Boolval(b) => {
    // SQLite uses 0/1 for booleans
    Ok(ast::Expr::Literal(ast::Literal::Numeric(
        if b.boolval { "1" } else { "0" }.to_string(),
    )))
}

// after
pg_query::protobuf::a_const::Val::Boolval(b) => Ok(ast::Expr::Literal(if b.boolval {
    ast::Literal::True
} else {
    ast::Literal::False
})),
```

- [ ] **Step 4: Run test to verify it passes, plus full regression**

Run: `cargo test -p turso_parser_pg test_boolean_literal_uses_true_false_variant -- --nocapture && cargo test -p turso_parser_pg && cargo test -p pgmicro`
Expected: PASS. Run the full crate suites since this touches every boolean literal translation path (defaults, CHECK constraints, WHERE clauses).

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): translate boolean literals to Literal::True/False, not Numeric 0/1"
```

---

### Task B7: Reject `DISTINCT ON` inside UNION/INTERSECT/EXCEPT branches instead of downgrading to plain DISTINCT (H4)

**Files:**
- Modify: `parser_pg/src/translator.rs:1864-1868` (`translate_one_select`)

**Root cause:** `translate_one_select` (the per-branch translator used by `translate_set_operation`) computes `distinctness` from `!select.distinct_clause.is_empty()` without checking whether the clause is `DISTINCT ON (...)` vs. plain `DISTINCT` (`Self::is_plain_distinct`, line 3919, already exists and checks for a single `None`-node entry). The top-level `translate_select` correctly special-cases `DISTINCT ON` via `wrap_distinct_on` (line 4004), but that rewrite needs a full `ast::Select` with its own `ORDER BY`, which a per-branch `OneSelect` inside a compound doesn't have. Per the "reject unsupported syntax with a clear error rather than silently produce wrong results" project rule, the minimal correct fix rejects instead of downgrading.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_distinct_on_in_union_branch_rejected_not_downgraded() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT DISTINCT ON (dept) dept, salary FROM emp ORDER BY dept, salary DESC \
               UNION SELECT dept, salary FROM contractors";
    let parsed = crate::parse(sql).unwrap();
    let err = translator.translate(&parsed).unwrap_err();
    assert!(matches!(err, ParseError::ParseError(_)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_distinct_on_in_union_branch_rejected_not_downgraded -- --nocapture`
Expected: FAIL (translation succeeds today, silently downgrading `DISTINCT ON (dept)` to plain `DISTINCT`).

- [ ] **Step 3: Implement**

```rust
// before (lines 1864-1868)
let distinctness = if !select.distinct_clause.is_empty() {
    Some(ast::Distinctness::Distinct)
} else {
    None
};

// after
let distinctness = if !select.distinct_clause.is_empty() {
    if !Self::is_plain_distinct(&select.distinct_clause) {
        return Err(ParseError::ParseError(
            "DISTINCT ON is not supported inside UNION/INTERSECT/EXCEPT branches".to_string(),
        ));
    }
    Some(ast::Distinctness::Distinct)
} else {
    None
};
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_distinct_on_in_union_branch_rejected_not_downgraded -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): reject DISTINCT ON inside compound SELECT branches instead of downgrading"
```

**Notes:** Full per-branch `DISTINCT ON` support (reusing `wrap_distinct_on`) is a legitimate follow-up feature, not required to close this bug — it would also need `translate_one_select` to start reading `select.sort_clause`/`select.limit_count`, which it currently ignores for compound branches entirely.

---

### Task B8: `ON CONFLICT` unrecognized action fails loud instead of silently dropping the clause (M2)

**Files:**
- Modify: `parser_pg/src/translator.rs:3811-3849` (`translate_on_conflict`)

- [ ] **Step 1: Implement** (no test required — see note below)

```rust
// before (line 3848)
_ => return Ok(None),

// after
other => {
    return Err(ParseError::ParseError(format!(
        "Unsupported ON CONFLICT action: {other:?}"
    )));
}
```

- [ ] **Step 2: Run full suite to confirm no regression**

Run: `cargo test -p turso_parser_pg`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): fail loud on unrecognized ON CONFLICT action instead of silently dropping the clause"
```

**Notes:** `OnConflictAction` only has `Undefined`/`OnconflictNone`/`OnconflictNothing`/`OnconflictUpdate` (confirmed via protobuf enum) — the catch-all only fires for `Undefined`/`OnconflictNone`, which shouldn't occur when `clause.action` is populated at all. This is defense-in-depth (fail loud, per project rule 12) rather than a fix for an observed real-world bug — no test is added since the branch is believed unreachable in practice; do not force a contrived test just to hit dead code.

---

### Task B9: Preserve expression-based `ON CONFLICT (lower(email))` targets (M3)

**Files:**
- Modify: `parser_pg/src/translator.rs:3852-3874` (`translate_on_conflict`)

**Interfaces:**
- Mirrors the existing pattern in `translate_create_index` (lines 764-768), which already handles `elem.name` vs `elem.expr` correctly for `CREATE INDEX ON t (lower(col))`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_on_conflict_expression_target_preserved() {
    let translator = PostgreSQLTranslator::new();
    let sql = "INSERT INTO users (email) VALUES ('A@B.com') \
               ON CONFLICT (lower(email)) DO NOTHING";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();
    if let ast::Stmt::Insert { body, .. } = translated {
        if let ast::InsertBody::Select(_, Some(upsert)) = body {
            let index = upsert.index.expect("Should have conflict target");
            assert_eq!(index.targets.len(), 1, "expression target was dropped");
        } else {
            panic!("Expected upsert");
        }
    } else {
        panic!("Expected Insert");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_on_conflict_expression_target_preserved -- --nocapture`
Expected: FAIL (`index.targets` is empty — the expression target was filtered out by `filter_map`'s implicit `None` for non-named elems).

- [ ] **Step 3: Implement**

```rust
// before (lines 3856-3873)
let targets: Vec<ast::SortedColumn> = infer
    .index_elems
    .iter()
    .filter_map(|elem| match &elem.node {
        Some(Node::IndexElem(idx_elem)) => {
            if !idx_elem.name.is_empty() {
                Some(ast::SortedColumn {
                    expr: Box::new(ast::Expr::Id(ast::Name::from_string(&idx_elem.name))),
                    order: None,
                    nulls: None,
                })
            } else {
                None
            }
        }
        _ => None,
    })
    .collect();

// after
let targets: Vec<ast::SortedColumn> = infer
    .index_elems
    .iter()
    .map(|elem| match &elem.node {
        Some(Node::IndexElem(idx_elem)) if !idx_elem.name.is_empty() => Ok(ast::SortedColumn {
            expr: Box::new(ast::Expr::Id(ast::Name::from_string(&idx_elem.name))),
            order: None,
            nulls: None,
        }),
        Some(Node::IndexElem(idx_elem)) => {
            let expr_node = idx_elem.expr.as_ref().ok_or_else(|| {
                ParseError::ParseError("ON CONFLICT target has no name or expression".into())
            })?;
            Ok(ast::SortedColumn {
                expr: Box::new(self.translate_expr(expr_node)?),
                order: None,
                nulls: None,
            })
        }
        _ => Err(ParseError::ParseError("ON CONFLICT: expected IndexElem".into())),
    })
    .collect::<Result<Vec<_>, ParseError>>()?;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_on_conflict_expression_target_preserved -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): preserve expression-based ON CONFLICT targets instead of dropping them"
```

---

### Task B10: Correctly translate `x <op> ANY/ALL (subquery)` for every comparison operator, not just `=` (C7)

**Files:**
- Modify: `parser_pg/src/translator.rs:3636-3679` (`translate_sublink`), add two new helpers near it

**Interfaces:**
- Produces: `fn translate_any_all_subquery(&self, sub_link: &pg_query::protobuf::SubLink, select: ast::Select, op: &str, is_all: bool) -> Result<ast::Expr, ParseError>`, `fn pg_comparison_operator(op: &str) -> Result<ast::Operator, ParseError>` (free function).
- Consumes: `ast::Expr::Exists`, `ast::SelectTable::Select`, `ast::FromClause` — all already exist and are used elsewhere (e.g. `build_numbered_subquery`, line 1666).

**Root cause:** `translate_sublink`'s `AnySublink` arm always calls `translate_in_subselect` regardless of the actual comparison operator:

```rust
SubLinkType::AnySublink => {
    // `x = ANY (subquery)` / `x IN (subquery)`
    self.translate_in_subselect(sub_link, select, false)
}
```

So `x > ANY (SELECT y FROM t)` silently becomes `x IN (SELECT y FROM t)` — wrong for every operator except `=`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_any_subquery_greater_than() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT * FROM orders WHERE total > ANY (SELECT limit_amt FROM limits)";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { where_clause, .. } = &select.body.select {
            let where_expr = where_clause.as_ref().expect("Should have WHERE");
            assert!(
                !matches!(**where_expr, ast::Expr::InSelect { .. }),
                "`> ANY` must not degrade to IN: {where_expr:?}"
            );
            if let ast::Expr::Exists(inner) = &**where_expr {
                if let ast::OneSelect::Select { where_clause, from, .. } = &inner.body.select {
                    assert!(from.is_some(), "EXISTS subquery must have a FROM");
                    let inner_where = where_clause.as_ref().expect("Should have inner WHERE");
                    assert!(
                        matches!(**inner_where, ast::Expr::Binary(_, ast::Operator::Greater, _)),
                        "Inner predicate should use > : {inner_where:?}"
                    );
                } else {
                    panic!("Expected simple SELECT inside EXISTS");
                }
            } else {
                panic!("Expected Exists expression, got: {where_expr:?}");
            }
        } else {
            panic!("Expected OneSelect::Select");
        }
    } else {
        panic!("Expected Select");
    }
}

#[test]
fn test_any_subquery_multi_column_rejected() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT * FROM orders WHERE total > ANY (SELECT a, b FROM limits)";
    let parsed = crate::parse(sql).unwrap();
    let err = translator.translate(&parsed).unwrap_err();
    assert!(matches!(err, ParseError::ParseError(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_parser_pg test_any_subquery_greater_than test_any_subquery_multi_column_rejected -- --nocapture`
Expected: FAIL — first test finds `ast::Expr::InSelect` instead of `Exists`; second test succeeds instead of erroring.

- [ ] **Step 3: Implement**

```rust
// before (translate_sublink, lines 3659-3662)
SubLinkType::AnySublink => {
    // `x = ANY (subquery)` / `x IN (subquery)`
    self.translate_in_subselect(sub_link, select, false)
}

// after
SubLinkType::AnySublink => {
    match Self::sublink_operator_name(sub_link) {
        Some("=") => self.translate_in_subselect(sub_link, select, false),
        Some(op) => self.translate_any_all_subquery(sub_link, select, op, false),
        None => Err(ParseError::ParseError("ANY subquery missing operator".to_string())),
    }
}
```

New helper (place after `translate_in_subselect`, ~line 3636):

```rust
/// Rewrite `x <op> ANY (subquery)` / `x <op> ALL (subquery)` (op != `=`/`<>`) as a
/// correlated EXISTS. Only supports a simple, non-compound subquery with exactly one
/// result column — anything else is rejected with a clear error rather than silently
/// mistranslated.
///
/// `x <op> ANY (subq)`  ⟺ `EXISTS (SELECT 1 FROM (subq) s WHERE x <op> s.col)`
/// `x <op> ALL (subq)`  ⟺ `NOT EXISTS (SELECT 1 FROM (subq) s WHERE NOT (x <op> s.col))`
fn translate_any_all_subquery(
    &self,
    sub_link: &pg_query::protobuf::SubLink,
    mut select: ast::Select,
    op: &str,
    is_all: bool,
) -> Result<ast::Expr, ParseError> {
    const SUB_ALIAS: &str = "__pgmicro_any_all";
    const COL_ALIAS: &str = "__pgmicro_any_all_col";

    let test_node = sub_link
        .testexpr
        .as_ref()
        .ok_or_else(|| ParseError::ParseError("ANY/ALL subquery missing testexpr".to_string()))?;
    let lhs = self.translate_expr(test_node)?;

    if !select.body.compounds.is_empty() {
        return Err(ParseError::ParseError(
            "ANY/ALL subquery with UNION/INTERSECT/EXCEPT is not supported".to_string(),
        ));
    }
    let ast::OneSelect::Select { columns, .. } = &mut select.body.select else {
        return Err(ParseError::ParseError(
            "ANY/ALL subquery must be a simple SELECT".to_string(),
        ));
    };
    if columns.len() != 1 {
        return Err(ParseError::ParseError(
            "ANY/ALL subquery must return exactly one column".to_string(),
        ));
    }
    match &mut columns[0] {
        ast::ResultColumn::Expr(_, alias) => {
            *alias = Some(ast::As::As(ast::Name::from_string(COL_ALIAS)));
        }
        _ => {
            return Err(ParseError::ParseError(
                "ANY/ALL subquery must return a single expression column".to_string(),
            ));
        }
    }

    let operator = pg_comparison_operator(op)?;
    let col_ref = ast::Expr::Qualified(
        ast::Name::from_string(SUB_ALIAS),
        ast::Name::from_string(COL_ALIAS),
    );
    let mut predicate = ast::Expr::Binary(Box::new(lhs), operator, Box::new(col_ref));
    if is_all {
        predicate = ast::Expr::Unary(ast::UnaryOperator::Not, Box::new(predicate));
    }

    let exists_select = ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns: vec![ast::ResultColumn::Expr(
                    Box::new(ast::Expr::Literal(ast::Literal::Numeric("1".to_string()))),
                    None,
                )],
                from: Some(ast::FromClause {
                    select: Box::new(ast::SelectTable::Select(
                        select,
                        Some(ast::As::As(ast::Name::from_string(SUB_ALIAS))),
                    )),
                    joins: vec![],
                }),
                where_clause: Some(Box::new(predicate)),
                group_by: None,
                window_clause: vec![],
            },
            compounds: vec![],
        },
        order_by: vec![],
        limit: None,
    };

    let exists = ast::Expr::Exists(exists_select);
    Ok(if is_all {
        ast::Expr::Unary(ast::UnaryOperator::Not, Box::new(exists))
    } else {
        exists
    })
}
```

New free-function operator mapping (place near `translate_binary_expr`'s inline match at line 2914):

```rust
fn pg_comparison_operator(op: &str) -> Result<ast::Operator, ParseError> {
    match op {
        "=" => Ok(ast::Operator::Equals),
        "<>" | "!=" => Ok(ast::Operator::NotEquals),
        "<" => Ok(ast::Operator::Less),
        "<=" => Ok(ast::Operator::LessEquals),
        ">" => Ok(ast::Operator::Greater),
        ">=" => Ok(ast::Operator::GreaterEquals),
        other => Err(ParseError::ParseError(format!(
            "Unsupported ANY/ALL comparison operator: {other}"
        ))),
    }
}
```

Before landing, verify `ast::UnaryOperator::Not` is the correct variant name for boolean negation (grep existing usage, e.g. in `translate_between_expr`'s sibling code).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_parser_pg test_any_subquery_greater_than test_any_subquery_multi_column_rejected -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): correctly translate x <op> ANY/ALL (subquery) for all comparison operators"
```

**Notes:** `AllSublink` (currently only supports `<>`) could be trivially upgraded to call `translate_any_all_subquery(..., is_all: true)` for other operators — a natural follow-up, not required to close C7.

---

### Task B11: Fix `~*`/`!~*` case-insensitive regex to use an inline `(?i)` flag (H6 — cross-workstream decision)

**Files:**
- Modify: `parser_pg/src/translator.rs:2836-2855` (`translate_binary_expr`)

**Cross-workstream note:** this is the decision documented in "Cross-workstream interfaces" item 2 above. The `(?i)`-prefix-via-Concat approach is **adopted**; a competing `regexp_i()` new-core-function approach proposed elsewhere is **rejected** — this fix requires zero `core/` changes. Verified `core/regexp.rs:1-29`: `regexp(pattern, haystack)` calls `regex::Regex::new(&pattern)` directly with no special-casing, and the Rust `regex` crate honors an inline `(?i)` flag anywhere in the pattern string — this works for both literal and dynamic (column/expression) patterns.

The existing comment at translator.rs:2836-2837 ("since SQLite REGEXP is case-insensitive by default") is **factually wrong** — `core/regexp.rs` has no `(?i)` and is case-sensitive by construction. Correct it as part of this fix.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_case_insensitive_regex_uses_inline_flag() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT * FROM users WHERE name ~* 'john'";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { where_clause, .. } = &select.body.select {
            if let ast::Expr::Like { op, rhs, .. } = &**where_clause.as_ref().unwrap() {
                assert!(matches!(op, ast::LikeOperator::Regexp));
                if let ast::Expr::Binary(l, ast::Operator::Concat, r) = &**rhs {
                    assert!(
                        matches!(&**l, ast::Expr::Literal(ast::Literal::String(s)) if s.contains("(?i)")),
                        "Expected '(?i)' prefix literal, got {l:?}"
                    );
                    assert!(matches!(&**r, ast::Expr::Literal(ast::Literal::String(s)) if s.contains("john")));
                } else {
                    panic!("Expected Concat rhs for case-insensitive regex, got {rhs:?}");
                }
            } else {
                panic!("Expected Like expression");
            }
        }
    } else {
        panic!("Expected Select");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_case_insensitive_regex_uses_inline_flag -- --nocapture`
Expected: FAIL (`~*` currently emits a plain `REGEXP` with no `(?i)` prefix, i.e. case-sensitive matching).

- [ ] **Step 3: Implement**

```rust
// before (lines 2836-2855)
// Case-insensitive regex (~*, !~*) — treat same as case-sensitive
// since SQLite REGEXP is case-insensitive by default
"~*" => {
    return Ok(ast::Expr::Like {
        lhs: left,
        not: false,
        op: ast::LikeOperator::Regexp,
        rhs: right,
        escape: None,
    });
}
"!~*" => {
    return Ok(ast::Expr::Like {
        lhs: left,
        not: true,
        op: ast::LikeOperator::Regexp,
        rhs: right,
        escape: None,
    });
}

// after
// Case-insensitive regex (~*, !~*) — Turso's `regexp()` (core/regexp.rs) calls
// regex::Regex::new() directly on the pattern with no implicit case-folding, so
// case-insensitivity must be requested explicitly via an inline `(?i)` flag,
// prepended at the SQL level so it also works for non-literal (dynamic) patterns.
"~*" | "!~*" => {
    let not = op_name == "!~*";
    let ci_pattern = ast::Expr::Binary(
        Box::new(ast::Expr::Literal(ast::Literal::String("'(?i)'".to_string()))),
        ast::Operator::Concat,
        right,
    );
    return Ok(ast::Expr::Like {
        lhs: left,
        not,
        op: ast::LikeOperator::Regexp,
        rhs: Box::new(ci_pattern),
        escape: None,
    });
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_case_insensitive_regex_uses_inline_flag -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): ~*/!~* case-insensitive regex now uses inline (?i) flag, was silently case-sensitive"
```

**Notes:** No cross-file dependency (contradicts an earlier assumption that a core change might be needed — verified `core/regexp.rs` before implementing, per the debugging protocol). Optional: add a one-line comment in `core/regexp.rs` noting pgmicro relies on inline-flag support, so a future change to strip/ignore inline flags there doesn't silently break this.

---

### Task B12: `DROP a, b, c` acts on every object, not just the first; `CASCADE` is rejected instead of silently ignored (H7)

**Files:**
- Modify: `parser_pg/src/translator.rs:130` (`translate_stmts` dispatch), `809-890` (`translate_drop`)

**Interfaces:**
- `translate_drop`'s return type changes from `Result<ast::Stmt, ParseError>` to `Result<Vec<ast::Stmt>, ParseError>` — grep confirmed the only call site is `translate_stmts:130` before making this change. `try_extract_drop_schema` (Task B14/H20) is a separate function reading `DropStmt` for `ObjectType::ObjectSchema` only, unaffected by this change.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_drop_multiple_tables_all_translated() {
    let translator = PostgreSQLTranslator::new();
    let sql = "DROP TABLE a, b, c";
    let parsed = crate::parse(sql).unwrap();
    let stmts = translator.translate_stmts(&parsed).unwrap();
    assert_eq!(stmts.len(), 3, "All 3 objects must be dropped, not just the first");
    let names: Vec<String> = stmts
        .iter()
        .map(|s| match s {
            ast::Stmt::DropTable { tbl_name, .. } => tbl_name.name.as_str().to_string(),
            other => panic!("Expected DropTable, got {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn test_drop_cascade_rejected_not_silently_ignored() {
    let translator = PostgreSQLTranslator::new();
    let sql = "DROP TABLE a CASCADE";
    let parsed = crate::parse(sql).unwrap();
    let err = translator.translate_stmts(&parsed).unwrap_err();
    assert!(matches!(err, ParseError::ParseError(_)));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_parser_pg test_drop_multiple_tables_all_translated test_drop_cascade_rejected_not_silently_ignored -- --nocapture`
Expected: FAIL — first test gets `stmts.len() == 1` (only `a` dropped); second succeeds instead of erroring.

- [ ] **Step 3: Implement**

```rust
// before (translate_stmts, line 130)
NodeRef::DropStmt(drop) => Ok(vec![self.translate_drop(drop)?]),

// after
NodeRef::DropStmt(drop) => self.translate_drop(drop),
```

`translate_drop` changes from returning one `ast::Stmt` (extracting `drop.objects.first()` once) to returning `Vec<ast::Stmt>` (mapping over every object):

```rust
fn translate_drop(&self, drop: &pg_query::protobuf::DropStmt) -> Result<Vec<ast::Stmt>, ParseError> {
    use pg_query::protobuf::{node::Node, DropBehavior, ObjectType};

    let remove_type = ObjectType::try_from(drop.remove_type)
        .map_err(|_| ParseError::ParseError("Invalid object type in DROP".into()))?;

    if DropBehavior::try_from(drop.behavior).ok() == Some(DropBehavior::DropCascade) {
        return Err(ParseError::ParseError(
            "DROP ... CASCADE is not supported; dependent objects must be dropped explicitly".into(),
        ));
    }

    if drop.objects.is_empty() {
        return Err(ParseError::ParseError("DROP missing object name".into()));
    }

    drop.objects
        .iter()
        .map(|obj_node| {
            // Existing per-object name-extraction logic (unchanged from before this task —
            // resolves a single DROP target's Node into an ast::QualifiedName, the same
            // logic previously run once against drop.objects.first()).
            let qualified_name = self.qualified_name_from_drop_object(obj_node)?;
            match remove_type {
                ObjectType::ObjectTable => Ok(ast::Stmt::DropTable {
                    if_exists: drop.missing_ok,
                    tbl_name: qualified_name,
                }),
                ObjectType::ObjectIndex => Ok(ast::Stmt::DropIndex {
                    if_exists: drop.missing_ok,
                    idx_name: qualified_name,
                }),
                ObjectType::ObjectView | ObjectType::ObjectMatview => Ok(ast::Stmt::DropView {
                    if_exists: drop.missing_ok,
                    view_name: qualified_name,
                }),
                ObjectType::ObjectType => Ok(ast::Stmt::DropType {
                    if_exists: drop.missing_ok,
                    type_name: qualified_name.name.as_str().to_string(),
                }),
                ObjectType::ObjectDomain => Ok(ast::Stmt::DropDomain {
                    if_exists: drop.missing_ok,
                    domain_name: qualified_name.name.as_str().to_string(),
                }),
                _ => Err(ParseError::ParseError(format!(
                    "DROP {remove_type:?} is not supported"
                ))),
            }
        })
        .collect()
}
```

`qualified_name_from_drop_object` is the pre-existing per-object `Node -> QualifiedName` extraction that today runs inline against `drop.objects.first()` (lines 822-862 of the pre-fix function) — factor it out into its own method with the same body, unchanged, so it can run once per object in the `.map()` above instead of once total.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_parser_pg test_drop_multiple_tables_all_translated test_drop_cascade_rejected_not_silently_ignored -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): DROP a, b, c drops every object not just the first; reject CASCADE instead of ignoring it"
```

**Notes:** `ast::Stmt::Drop*` variants (`DropTable`/`DropIndex`/`DropView`/`DropType`/`DropDomain`, `parser/src/ast.rs:235-275`) have no CASCADE representation — real cascading-drop support needs new AST + core support and is a two-plan candidate, not scheduled here. RESTRICT (PG's default) needs no special handling since it's the existing behavior.

---

### Task B13: Reject `ON CONFLICT ON CONSTRAINT name` instead of silently broadening to an unqualified upsert (H10)

**Files:**
- Modify: `parser_pg/src/translator.rs:3852-3890` (`translate_on_conflict`)

**Current bug severity:** when `infer.conname` is set (`ON CONSTRAINT` syntax), `infer.index_elems` is empty, so the current code's `if !infer.index_elems.is_empty() { ... } else { None }` sets `index: None` — the upsert silently becomes an *unqualified* `ON CONFLICT DO UPDATE`, applying on any conflict rather than just the named constraint. That's a silent semantic broadening, worth rejecting rather than emitting.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_on_conflict_on_constraint_rejected_not_silently_broadened() {
    let translator = PostgreSQLTranslator::new();
    let sql = "INSERT INTO users (id, email) VALUES (1, 'a@b.com') \
               ON CONFLICT ON CONSTRAINT users_email_key DO UPDATE SET email = excluded.email";
    let parsed = crate::parse(sql).unwrap();
    let err = translator.translate(&parsed).unwrap_err();
    assert!(matches!(err, ParseError::ParseError(_)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_on_conflict_on_constraint_rejected_not_silently_broadened -- --nocapture`
Expected: FAIL (translation succeeds today, producing an unqualified upsert).

- [ ] **Step 3: Implement**

```rust
// before (translate_on_conflict, lines 3851-3852)
// Translate conflict target (the columns in ON CONFLICT (col1, col2))
let index = if let Some(infer) = &clause.infer {

// after
// Translate conflict target (the columns in ON CONFLICT (col1, col2))
let index = if let Some(infer) = &clause.infer {
    if !infer.conname.is_empty() {
        return Err(ParseError::ParseError(format!(
            "ON CONFLICT ON CONSTRAINT {} is not supported (no schema access at translate \
             time to resolve the constraint's columns); use ON CONFLICT (columns) instead",
            infer.conname
        )));
    }
```

(The rest of the existing `if let Some(infer) = &clause.infer { ... }` body is unchanged, just gains this new leading check.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_on_conflict_on_constraint_rejected_not_silently_broadened -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): reject ON CONFLICT ON CONSTRAINT instead of silently broadening to unqualified upsert"
```

**Two-plan note:** real support needs `PostgreSQLTranslator` (currently stateless, `#[derive(Default)]`, with a `// TODO: Add schema information` comment at translator.rs:37) to gain a schema-lookup hook threaded through `translate()`/`translate_stmts()` to resolve constraint names to columns — an architectural change, out of scope here.

---

### Task B14: `CAST` to an unmapped/user-defined type no longer silently drops the cast; type params and array dimensions are preserved (H3, covers H12)

**Files:**
- Modify: `parser_pg/src/translator.rs:4714-4747` (`pg_type_name_to_ast_type`), `4810-4829` (`extract_integer_typmods`)

**Interfaces:**
- Produces: `fn extract_typmods_from_type_name(type_name: &pg_query::protobuf::TypeName) -> Vec<i64>`.
- Consumes: `fn map_pg_type(pg_type: &str, typmods: &[i64]) -> Option<PgTypeMapping>` (already exists, line 4460, the canonical type-mapping function — this task removes a second, divergent, hand-maintained ~20-arm type table that duplicated it incompletely).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_cast_to_unmapped_enum_type_preserved() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT status::my_enum_type FROM orders";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { columns, .. } = &select.body.select {
            if let ast::ResultColumn::Expr(expr, _) = &columns[0] {
                if let ast::Expr::Cast { type_name, .. } = &**expr {
                    let ty = type_name.as_ref().expect(
                        "CAST to unknown/user-defined type must not be silently dropped",
                    );
                    assert_eq!(ty.name, "my_enum_type");
                } else {
                    panic!("Expected Cast expression, got {expr:?}");
                }
            } else {
                panic!("Expected expr result column");
            }
        }
    } else {
        panic!("Expected Select");
    }
}

#[test]
fn test_cast_preserves_varchar_length_and_array_dims() {
    let translator = PostgreSQLTranslator::new();
    let sql = "SELECT name::varchar(3), tags::integer[] FROM t";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();

    if let ast::Stmt::Select(select) = translated {
        if let ast::OneSelect::Select { columns, .. } = &select.body.select {
            if let ast::ResultColumn::Expr(expr, _) = &columns[0] {
                if let ast::Expr::Cast { type_name, .. } = &**expr {
                    let ty = type_name.as_ref().expect("varchar(3) cast type dropped");
                    assert!(ty.size.is_some(), "varchar(3) length param was dropped");
                } else {
                    panic!("Expected Cast expression, got {expr:?}");
                }
            }
            if let ast::ResultColumn::Expr(expr, _) = &columns[1] {
                if let ast::Expr::Cast { type_name, .. } = &**expr {
                    let ty = type_name.as_ref().expect("integer[] cast type dropped");
                    assert_eq!(ty.array_dimensions, 1, "array dimension was dropped");
                } else {
                    panic!("Expected Cast expression, got {expr:?}");
                }
            }
        } else {
            panic!("Expected OneSelect::Select");
        }
    } else {
        panic!("Expected Select");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_parser_pg test_cast_to_unmapped_enum_type_preserved test_cast_preserves_varchar_length_and_array_dims -- --nocapture`
Expected: FAIL — first test: `type_name` is `None` (cast silently dropped to a bare passthrough of the inner expr); second test: `size`/`array_dimensions` are `None`/`0`.

- [ ] **Step 3: Implement**

```rust
// before (lines 4714-4747)
fn pg_type_name_to_ast_type(type_name: &pg_query::protobuf::TypeName) -> Option<ast::Type> {
    let pg_type = pg_typename_base(type_name)?;

    let name = match pg_type.as_str() {
        "INTEGER" | "INT" | "INT4" | "SERIAL" | "BIGSERIAL" | "SMALLSERIAL" | "OID"
        | "REGCLASS" | "REGTYPE" => "INTEGER",
        // ... (divergent hand-maintained table, ~20 arms) ...
        "BYTEA" | "BLOB" => "BLOB",
        _ => return None,
    };

    Some(ast::Type {
        name: name.to_string(),
        size: None,
        array_dimensions: 0,
    })
}

// after
fn pg_type_name_to_ast_type(type_name: &pg_query::protobuf::TypeName) -> Option<ast::Type> {
    let pg_type = pg_typename_base(type_name)?;
    let typmods = extract_typmods_from_type_name(type_name);
    let mapping = map_pg_type(&pg_type, &typmods)?;

    let size = match mapping.type_params.as_slice() {
        [p, s] => Some(ast::TypeSize::TypeSize(
            Box::new(ast::Expr::Literal(ast::Literal::Numeric(p.to_string()))),
            Box::new(ast::Expr::Literal(ast::Literal::Numeric(s.to_string()))),
        )),
        [n] => Some(ast::TypeSize::MaxSize(Box::new(ast::Expr::Literal(
            ast::Literal::Numeric(n.to_string()),
        )))),
        _ => None,
    };

    Some(ast::Type {
        name: mapping.type_name,
        size,
        array_dimensions: mapping.array_dimensions,
    })
}
```

New shared helper, extracted from the existing `ColumnDef`-only `extract_integer_typmods` since `TypeName` is the common inner data both call sites need:

```rust
fn extract_typmods_from_type_name(type_name: &pg_query::protobuf::TypeName) -> Vec<i64> {
    use pg_query::protobuf::a_const::Val;
    use pg_query::protobuf::node::Node;

    type_name
        .typmods
        .iter()
        .filter_map(|node| match &node.node {
            Some(Node::Integer(i)) => Some(i.ival as i64),
            Some(Node::AConst(a_const)) => match &a_const.val {
                Some(Val::Ival(i)) => Some(i.ival as i64),
                _ => None,
            },
            _ => None,
        })
        .collect()
}
```

Simplify the existing `extract_integer_typmods` (line 4810-4829) to delegate:

```rust
fn extract_integer_typmods(col_def: &pg_query::protobuf::ColumnDef) -> Vec<i64> {
    col_def
        .type_name
        .as_ref()
        .map(extract_typmods_from_type_name)
        .unwrap_or_default()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_parser_pg test_cast_to_unmapped_enum_type_preserved test_cast_preserves_varchar_length_and_array_dims -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): CAST to unmapped/user-defined types preserves the type name, params, and array dims"
```

**Notes:** This single edit closes both H3 (unknown types now pass through, matching `map_pg_type`'s existing `_ => pg_type.to_lowercase()` fallback) and H12 (params/array dims now flow through via `mapping.type_params`/`mapping.array_dimensions` instead of being hardcoded away).

---

### Task B15: `DROP SCHEMA a, b, c` extracts every schema name, not just the first (H20 — cross-workstream interface)

**Files:**
- Modify: `parser_pg/src/translator.rs:4989-4993` (`PgDropSchemaStmt` struct), `5082-5113` (`try_extract_drop_schema`)

**Cross-workstream note:** this is the documented B→C interface (see "Cross-workstream interfaces" item 1 above). This task does **not** touch `core/pg_dispatch.rs` — updating `handle_pg_drop_schema` (and its call site at `core/pg_dispatch.rs:146`) to iterate `stmt.names` instead of reading `stmt.name` is explicitly Workstream C's responsibility. This task will not compile against `core/pg_dispatch.rs` until that consumer update lands — coordinate landing order with Workstream C, or land both in the same PR.

**Interfaces:**
- Produces (after this task): `PgDropSchemaStmt.name: String` becomes `PgDropSchemaStmt.names: Vec<String>` — never empty, guaranteed by `try_extract_drop_schema` returning `None` if `drop.objects` is empty. `if_exists`/`cascade` remain single top-level fields (PG's `DROP SCHEMA a, b, c CASCADE` applies one shared `IF EXISTS`/`CASCADE` to the whole statement, not per-name — confirmed via `pg_query`'s `DropStmt` shape).
- Consumes: Workstream C updates `handle_pg_drop_schema` to loop over `stmt.names`, calling the existing per-name logic once per entry.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn test_drop_schema_multiple_names_extracted() {
    let sql = "DROP SCHEMA a, b, c";
    let parsed = crate::parse(sql).unwrap();
    let stmt = try_extract_drop_schema(&parsed).expect("Should extract DROP SCHEMA");
    assert_eq!(stmt.names, vec!["a", "b", "c"]);
    assert!(!stmt.if_exists);
    assert!(!stmt.cascade);
}

#[test]
fn test_drop_schema_single_name_still_works() {
    let sql = "DROP SCHEMA IF EXISTS myschema CASCADE";
    let parsed = crate::parse(sql).unwrap();
    let stmt = try_extract_drop_schema(&parsed).expect("Should extract DROP SCHEMA");
    assert_eq!(stmt.names, vec!["myschema"]);
    assert!(stmt.if_exists);
    assert!(stmt.cascade);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p turso_parser_pg test_drop_schema_multiple_names_extracted test_drop_schema_single_name_still_works -- --nocapture`
Expected: FAIL to compile — `PgDropSchemaStmt` has no `names` field yet.

- [ ] **Step 3: Implement**

```rust
// before (lines 4989-4993)
pub struct PgDropSchemaStmt {
    pub name: String,
    pub if_exists: bool,
    pub cascade: bool,
}

// after
pub struct PgDropSchemaStmt {
    /// Schema names to drop. Always non-empty. PG's `DROP SCHEMA a, b, c` shares one
    /// `IF EXISTS`/`CASCADE` across all names, not per-name.
    pub names: Vec<String>,
    pub if_exists: bool,
    pub cascade: bool,
}
```

```rust
// before (try_extract_drop_schema, lines 5082-5113)
pub fn try_extract_drop_schema(parse_result: &ParseResult) -> Option<PgDropSchemaStmt> {
    use pg_query::protobuf::{DropBehavior, ObjectType};
    use pg_query::NodeRef;

    let nodes = parse_result.protobuf.nodes();
    if nodes.is_empty() {
        return None;
    }
    let NodeRef::DropStmt(drop) = &nodes[0].0 else {
        return None;
    };
    let remove_type = ObjectType::try_from(drop.remove_type).ok()?;
    if remove_type != ObjectType::ObjectSchema {
        return None;
    }

    // Extract schema name from first object (String node)
    let obj = drop.objects.first()?;
    let obj_node = obj.node.as_ref()?;
    let name = match obj_node.to_ref() {
        NodeRef::String(s) => s.sval.clone(),
        _ => return None,
    };

    let cascade = DropBehavior::try_from(drop.behavior).ok() == Some(DropBehavior::DropCascade);

    Some(PgDropSchemaStmt {
        name,
        if_exists: drop.missing_ok,
        cascade,
    })
}

// after
pub fn try_extract_drop_schema(parse_result: &ParseResult) -> Option<PgDropSchemaStmt> {
    use pg_query::protobuf::{DropBehavior, ObjectType};
    use pg_query::NodeRef;

    let nodes = parse_result.protobuf.nodes();
    if nodes.is_empty() {
        return None;
    }
    let NodeRef::DropStmt(drop) = &nodes[0].0 else {
        return None;
    };
    let remove_type = ObjectType::try_from(drop.remove_type).ok()?;
    if remove_type != ObjectType::ObjectSchema {
        return None;
    }
    if drop.objects.is_empty() {
        return None;
    }

    let names: Vec<String> = drop
        .objects
        .iter()
        .filter_map(|obj| match obj.node.as_ref()?.to_ref() {
            NodeRef::String(s) => Some(s.sval.clone()),
            _ => None,
        })
        .collect();
    if names.len() != drop.objects.len() {
        // A non-String node in the list means something we don't understand — bail
        // rather than silently dropping the un-parseable name.
        return None;
    }

    let cascade = DropBehavior::try_from(drop.behavior).ok() == Some(DropBehavior::DropCascade);

    Some(PgDropSchemaStmt {
        names,
        if_exists: drop.missing_ok,
        cascade,
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p turso_parser_pg test_drop_schema_multiple_names_extracted test_drop_schema_single_name_still_works -- --nocapture`
Expected: Compiles and PASSes only once Workstream C's `handle_pg_drop_schema` consumer update has also landed (this crate's own tests compile independently of `core/`, but the workspace build will fail until both sides land — verify with `cargo build --workspace` before considering this task done in a shared branch).

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): DROP SCHEMA a, b, c extracts every schema name, not just the first"
```

**Notes:** Do not attempt to fix `handle_pg_drop_schema` in `core/pg_dispatch.rs` from this workstream — that's explicitly Workstream C's task per the coordinator's note in "Cross-workstream interfaces" item 1.

---

### Task B16 (deferred, two-plan): `@>`/`<@`/`&&` unconditionally mapped to array functions, breaking JSONB containment (H5)

**Do not implement as a bite-sized task.** Flagged as a design/two-plan item, not scheduled for direct execution in this plan.

**Files:** `parser_pg/src/translator.rs:2936-2953` (`translate_binary_expr`) — parser_pg side only, size S. Cross-file: `core/functions/postgres.rs`, `core/function.rs` — core side, size M-L.

**Why deferred:** the parser_pg side is a trivial rename (`array_contains_all`/`array_overlap` → `pg_contains`/`pg_overlaps`), but the translator has no schema/type information at translate time — it is a pure syntax-to-syntax transform. Real correctness requires new core scalar functions `pg_contains`/`pg_overlaps` that dispatch on the *runtime* value type (array vs. JSON/JSONB), which is core work. Confirmed `core/function.rs` currently registers `ArrayOverlap`/`ArrayContainsAll` as array-only scalar functions with no JSONB-aware branch.

**Reference translation (for whoever picks up the two-plan work — do not land in isolation):**

```rust
// before (lines 2936-2953)
"@>" | "<@" | "&&" => {
    let (func_name, args) = match op_name {
        "@>" => ("array_contains_all", vec![left, right]),
        "<@" => ("array_contains_all", vec![right, left]), // swap: b contains all of a
        "&&" => ("array_overlap", vec![left, right]),
        _ => unreachable!(),
    };
    return Ok(ast::Expr::FunctionCall {
        name: ast::Name::from_string(func_name),
        distinctness: None,
        args,
        order_by: vec![],
        filter_over: ast::FunctionTail { filter_clause: None, over_clause: None },
    });
}

// after (only once pg_contains/pg_overlaps exist in core)
"@>" | "<@" | "&&" => {
    let (func_name, args) = match op_name {
        "@>" => ("pg_contains", vec![left, right]),
        "<@" => ("pg_contains", vec![right, left]),
        "&&" => ("pg_overlaps", vec![left, right]),
        _ => unreachable!(),
    };
    return Ok(ast::Expr::FunctionCall {
        name: ast::Name::from_string(func_name),
        distinctness: None,
        args,
        order_by: vec![],
        filter_over: ast::FunctionTail { filter_clause: None, over_clause: None },
    });
}
```

**Critical:** do not land the parser_pg rename before `pg_contains`/`pg_overlaps` exist in core, or every existing array `@>`/`<@`/`&&` query breaks. Two-plan candidate per CLAUDE.md: (1) a Turso-core plan for polymorphic containment functions, self-justifying with no mention of Postgres, (2) a pgmicro plan for this rename once the core functions land.

---

### Task B17: Reject `DELETE ... USING` instead of silently ignoring the join filter (H8)

**Files:**
- Modify: `parser_pg/src/translator.rs:1253-1283` (`translate_delete`)

**Current bug severity:** silently ignoring `USING` means the DELETE runs with only the `WHERE` clause and no join filter — it can delete far more rows than intended. E.g. `DELETE FROM t USING u WHERE t.id = u.id AND u.flag = true` currently deletes **every row of `t`**, since `u.flag` and the join condition vanish. This is a data-loss-risk bug, so the minimal safe fix is outright rejection, not a partial rewrite.

**Cross-file note:** real support is a two-plan candidate — `ast::Stmt::Delete` (`parser/src/ast.rs:213-228`) has no `from`/`using` field (confirmed via grep: only `with`, `tbl_name`, `indexed`, `where_clause`, `returning`, `order_by`, `limit`), and SQLite has no native multi-table DELETE, so real support needs a new AST field plus core VDBE-level join support for DELETE that does not exist today. Not achievable purely in parser_pg — not scheduled here.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_delete_using_rejected_not_silently_ignored() {
    let translator = PostgreSQLTranslator::new();
    let sql = "DELETE FROM orders USING customers WHERE orders.cust_id = customers.id AND customers.banned";
    let parsed = crate::parse(sql).unwrap();
    let err = translator.translate(&parsed).unwrap_err();
    assert!(
        matches!(err, ParseError::ParseError(_)),
        "DELETE ... USING must be rejected, not silently executed without the join filter"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_delete_using_rejected_not_silently_ignored -- --nocapture`
Expected: FAIL (translation succeeds today, silently dropping the `USING customers` clause and its join condition).

- [ ] **Step 3: Implement**

```rust
// before (translate_delete, lines 1253-1262)
fn translate_delete(
    &self,
    delete: &pg_query::protobuf::DeleteStmt,
) -> Result<ast::Stmt, ParseError> {
    // Extract table name
    let relation = delete
        .relation
        .as_ref()
        .ok_or_else(|| ParseError::ParseError("DELETE missing target table".into()))?;
    let tbl_name = self.qualified_name_from_range_var(relation);

// after
fn translate_delete(
    &self,
    delete: &pg_query::protobuf::DeleteStmt,
) -> Result<ast::Stmt, ParseError> {
    if !delete.using_clause.is_empty() {
        return Err(ParseError::ParseError(
            "DELETE ... USING is not supported (Turso has no multi-table DELETE); \
             rewrite using a correlated WHERE ... IN/EXISTS subquery instead".into(),
        ));
    }

    // Extract table name
    let relation = delete
        .relation
        .as_ref()
        .ok_or_else(|| ParseError::ParseError("DELETE missing target table".into()))?;
    let tbl_name = self.qualified_name_from_range_var(relation);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_delete_using_rejected_not_silently_ignored -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): reject DELETE ... USING instead of silently dropping the join filter (data-loss risk)"
```

**Notes:** A fuller fix (rewriting the common single-USING-table, no-RETURNING-of-USING-columns case to `WHERE EXISTS (SELECT 1 FROM u WHERE <join+extra predicates>)`) is possible entirely within parser_pg without core changes — `ast::Expr::Exists` and `FromClause` machinery already exist. Flagged as a worthwhile follow-up, but the immediate safety fix should land first since it eliminates the silent-data-loss risk today.

---

### Task B18 (Medium, cross-file dependency check first): `CHAR(n)` mapped to the same custom type as `VARCHAR(n)` (M4)

**Files:** Modify: `parser_pg/src/translator.rs:4497-4502` (`map_pg_type`)

- [ ] **Step 1: Check whether a `"char"` Turso custom type exists.** Grep `core/` for where `"varchar"` is registered as a custom type; if `"char"` doesn't exist alongside it, this task needs a `core/` change first (cross-file, size M) — do not proceed with the parser_pg edit below until that's confirmed either way.

```rust
// before
"VARCHAR" | "CHAR" => {
    return match params.first() {
        Some(_) => Some(PgTypeMapping::with_params("varchar", params.to_vec())),
        None => Some(PgTypeMapping::scalar("TEXT")),
    };
}

// after (once "char" exists as a distinct Turso custom type)
"VARCHAR" => {
    return match params.first() {
        Some(_) => Some(PgTypeMapping::with_params("varchar", params.to_vec())),
        None => Some(PgTypeMapping::scalar("TEXT")),
    };
}
"CHAR" => {
    return match params.first() {
        Some(_) => Some(PgTypeMapping::with_params("char", params.to_vec())),
        None => Some(PgTypeMapping::scalar("TEXT")),
    };
}
```

**Interim alternative if `"char"` doesn't exist and the core work isn't worth it yet:** leave `CHAR(n)` mapped to `"varchar"`, but add a code comment documenting that PG's blank-padding-on-read semantics for `bpchar` won't be honored — do not silently claim this is fixed.

- [ ] **Step 2: Commit** (whichever path taken)

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): give CHAR(n) its own type mapping distinct from VARCHAR(n)"
```

---

### Task B19 (Medium, cross-file dependency check first): `TIME`/`TIMETZ` collapse to one type (M5)

**Files:** Modify: `parser_pg/src/translator.rs:4488` (`map_pg_type`), `4726` (now the shared path after Task B14's fix)

Same shape as B18 — check whether `"timetz"` exists as a distinct Turso custom type (mirroring `"timestamp"` vs `"timestamptz"`, already distinct at translator.rs:4489-4490) before landing:

```rust
// before
"TIME" | "TIMETZ" => "time".into(),

// after (once "timetz" exists as a distinct Turso custom type)
"TIME" => "time".into(),
"TIMETZ" => "timetz".into(),
```

- [ ] **Commit** (once the core-side type exists)

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): TIMETZ maps to its own type, no longer collapsed into TIME"
```

---

### Task B20 (documentation only, no code change): Bare `NUMERIC` defaulting to `numeric(38,19)` is intentional (M6)

**Files:** `parser_pg/src/translator.rs:4503-4509` (`map_pg_type`)

Investigation confirms this is intentional, already-tested behavior (`test_type_mapping`, lines 5968-5971), not an oversight — PG's bare `NUMERIC` is unbounded-precision, and `(38,19)` is a deliberate bounded approximation matching common RDBMS defaults, needed because Turso's `numeric` custom type requires a fixed precision/scale. No product decision to change this was made during this review, so the only change is a clarifying comment:

```rust
// after
"NUMERIC" | "DECIMAL" => {
    return match params {
        [p, s] => Some(PgTypeMapping::with_params("numeric", vec![*p, *s])),
        [p] => Some(PgTypeMapping::with_params("numeric", vec![*p, 0])),
        // PG's bare NUMERIC is unbounded precision; Turso's numeric custom type
        // requires a fixed precision/scale. (38, 19) is a deliberate approximation
        // (matches common RDBMS defaults), not a bug — see EVALUATION.md / this note.
        _ => Some(PgTypeMapping::with_params("numeric", vec![38, 19])),
    };
}
```

- [ ] **Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "docs(translator): document bare NUMERIC's (38,19) default as intentional, not a bug"
```

---

### Task B21: `def_elem_bool_val` fails loud on unrecognized boolean strings instead of treating them as `false` (M7)

**Files:** Modify: `parser_pg/src/translator.rs:4396-4408` (`def_elem_bool_val`), call sites at `4284`, `5201`, `5355`

**Land together with Task B22 (M8)** — both touch the same call sites; doing them separately means touching each call site twice.

- [ ] **Step 1: Implement**

```rust
// before
fn def_elem_bool_val(def: &pg_query::protobuf::DefElem) -> Option<bool> {
    let arg = def.arg.as_deref()?;
    match &arg.node {
        Some(pg_query::protobuf::node::Node::Integer(i)) => Some(i.ival != 0),
        Some(pg_query::protobuf::node::Node::String(s)) => Some(matches!(
            s.sval.to_lowercase().as_str(),
            "true" | "on" | "1"
        )),
        _ => None,
    }
}

// after
fn def_elem_bool_val(def: &pg_query::protobuf::DefElem) -> Result<Option<bool>, ParseError> {
    let Some(arg) = def.arg.as_deref() else {
        return Ok(None);
    };
    match &arg.node {
        Some(pg_query::protobuf::node::Node::Integer(i)) => Ok(Some(i.ival != 0)),
        Some(pg_query::protobuf::node::Node::String(s)) => {
            match s.sval.to_lowercase().as_str() {
                "true" | "on" | "1" | "yes" => Ok(Some(true)),
                "false" | "off" | "0" | "no" => Ok(Some(false)),
                other => Err(ParseError::ParseError(format!(
                    "invalid boolean value for option {}: {other:?}",
                    def.defname
                ))),
            }
        }
        _ => Ok(None),
    }
}
```

Call sites (e.g. `translate_copy:4284`) change from `def_elem_bool_val(def).unwrap_or(true)` to `def_elem_bool_val(def)?.unwrap_or(true)` — these are the same call sites Task B22 consolidates, so implement both tasks in the same commit.

- [ ] **Step 2: Run full suite**

Run: `cargo test -p turso_parser_pg`
Expected: PASS.

- [ ] **Step 3: Commit** — see Task B22's commit, which bundles both.

---

### Task B22: Consolidate 4 duplicated COPY-option extractors into one shared parser, wiring in the previously-ignored `ENCODING` option (M8)

**Files:** Modify: `parser_pg/src/translator.rs:4269-4290` (`translate_copy`), `5187-5205` (`try_extract_copy_from`), `5340-5359` (`try_extract_copy_stdin`), `5427-5453` (`try_extract_copy_stdout`)

**Confirmed duplication:** all four functions independently loop over `copy.options` matching on `def.defname.as_str()` for `"format"`/`"delimiter"`/`"header"`/`"null"` — `try_extract_copy_stdout`'s loop omits `"header"` entirely (so `COPY ... TO STDOUT WITH (HEADER)` currently can't emit a header row on export). None handle `"encoding"`. `translate_copy` additionally handles `"quote"`/`"escape"`, which the other three don't.

- [ ] **Step 1: Implement the shared struct + parser**

```rust
/// Parsed COPY option set, shared by translate_copy and the three try_extract_copy_*
/// functions (which previously duplicated this loop with different subsets of options).
#[derive(Default)]
struct PgCopyOptions {
    format: Option<String>,   // "text" | "csv" | "binary"
    delimiter: Option<String>,
    header: Option<bool>,
    null_string: Option<String>,
    quote: Option<String>,
    escape: Option<String>,
    encoding: Option<String>,
}

fn parse_copy_options(
    options: &[pg_query::protobuf::Node],
) -> Result<PgCopyOptions, ParseError> {
    let mut opts = PgCopyOptions::default();
    for opt in options {
        let Some(pg_query::protobuf::node::Node::DefElem(def)) = &opt.node else {
            continue;
        };
        match def.defname.as_str() {
            "format" => opts.format = def_elem_string_val(def),
            "delimiter" => opts.delimiter = def_elem_string_val(def),
            "header" => opts.header = def_elem_bool_val(def)?,
            "null" => opts.null_string = def_elem_string_val(def),
            "quote" => opts.quote = def_elem_string_val(def),
            "escape" => opts.escape = def_elem_string_val(def),
            "encoding" => opts.encoding = def_elem_string_val(def),
            _ => {}
        }
    }
    Ok(opts)
}
```

Each of the four call sites replaces its bespoke loop with `let opts = parse_copy_options(&copy.options)?;` and reads the fields it needs — e.g. `try_extract_copy_stdout` gains `header: opts.header.unwrap_or(false)` where it previously had none.

- [ ] **Step 2: Verify the `encoding` field's downstream consumer before wiring it through further**

Check `core/pg_dispatch.rs::handle_pg_copy_from` to confirm whether the connection layer actually transcodes based on `encoding`, or whether it's captured here but still ignored one layer down. If the latter, note it as a follow-up cross-file gap rather than claiming full support.

- [ ] **Step 3: Run full suite**

Run: `cargo test -p turso_parser_pg`
Expected: PASS.

- [ ] **Step 4: Commit** (bundles Task B21's `def_elem_bool_val` fallibility change, since both touch the same call sites)

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): consolidate duplicated COPY option extractors, wire ENCODING option, fail loud on invalid booleans"
```

**Notes:** `PgCopyFromStmt`/`PgCopyStdinStmt`/`PgCopyStdoutStmt` structs need an `encoding` field added to carry the parsed value through to `core/pg_dispatch.rs`.

---

### Task B23: `TRUNCATE ... CASCADE` is rejected instead of silently ignored (M9)

**Files:** Modify: `parser_pg/src/translator.rs:892-921` (`translate_truncate`), `5244-5274` (`try_extract_truncate`)

Confirmed `TruncateStmt.restart_seqs: bool` and `TruncateStmt.behavior: DropBehavior` (protobuf) are never read in either function.

- [ ] **Step 1: Implement**

```rust
// before (translate_truncate, lines 892-902)
fn translate_truncate(
    &self,
    truncate: &pg_query::protobuf::TruncateStmt,
) -> Result<ast::Stmt, ParseError> {
    // TRUNCATE TABLE t → DELETE FROM t (single-table only; multi-table handled in
    // try_prepare_pg via sequential DELETEs).
    if truncate.relations.len() > 1 {
        return Err(ParseError::ParseError(
            "multi-table TRUNCATE must be handled by the connection layer".into(),
        ));
    }

// after
fn translate_truncate(
    &self,
    truncate: &pg_query::protobuf::TruncateStmt,
) -> Result<ast::Stmt, ParseError> {
    use pg_query::protobuf::DropBehavior;
    if truncate.relations.len() > 1 {
        return Err(ParseError::ParseError(
            "multi-table TRUNCATE must be handled by the connection layer".into(),
        ));
    }
    if DropBehavior::try_from(truncate.behavior).ok() == Some(DropBehavior::DropCascade) {
        return Err(ParseError::ParseError(
            "TRUNCATE ... CASCADE is not supported".into(),
        ));
    }
    // truncate.restart_seqs (RESTART IDENTITY) is not yet honored — see Notes.
```

Apply the identical `behavior`/`restart_seqs` checks to `try_extract_truncate` (the multi-table path, consumed by `core/pg_dispatch.rs::handle_pg_truncate` — flag that RESTART IDENTITY support there is a cross-file follow-up if picked up later).

- [ ] **Step 2: Run full suite**

Run: `cargo test -p turso_parser_pg`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): reject TRUNCATE ... CASCADE instead of silently ignoring it"
```

**Notes:** `RESTART IDENTITY` support (resetting `sqlite_sequence` for the table) needs `translate_truncate` to emit a second statement, changing its return type from `ast::Stmt` to `Vec<ast::Stmt>` (same pattern as Task B12/H7) — not scheduled here, flagged as a follow-up.

---

### Task B24: `CREATE OR REPLACE VIEW` honors `REPLACE`; `WITH NO DATA` is rejected instead of silently creating a populated view (M10)

**Files:** Modify: `parser_pg/src/translator.rs:130-140` (`translate_stmts` dispatch for `ViewStmt`), `923-969` (`translate_create_view`), `973-1031` (`translate_create_table_as`)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_create_or_replace_view_emits_drop_then_create() {
    let translator = PostgreSQLTranslator::new();
    let sql = "CREATE OR REPLACE VIEW v AS SELECT 1";
    let parsed = crate::parse(sql).unwrap();
    let stmts = translator.translate_stmts(&parsed).unwrap();
    assert_eq!(stmts.len(), 2);
    assert!(matches!(stmts[0], ast::Stmt::DropView { if_exists: true, .. }));
    assert!(matches!(stmts[1], ast::Stmt::CreateView { .. }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_create_or_replace_view_emits_drop_then_create -- --nocapture`
Expected: FAIL (`stmts.len() == 1`, `view.replace` is never read).

- [ ] **Step 3: Implement**

```rust
// before (translate_stmts, line 136)
NodeRef::ViewStmt(view) => Ok(vec![self.translate_create_view(view)?]),

// after
NodeRef::ViewStmt(view) => {
    let create = self.translate_create_view(view)?;
    if view.replace {
        let view_name = match &create {
            ast::Stmt::CreateView { view_name, .. } => view_name.clone(),
            _ => unreachable!(),
        };
        Ok(vec![
            ast::Stmt::DropView { if_exists: true, view_name },
            create,
        ])
    } else {
        Ok(vec![create])
    }
}
```

For `WITH NO DATA` (`IntoClause.skip_data: bool` in `ctas.into`, read nowhere in `translate_create_table_as`): `ast::Stmt::CreateMaterializedView` has no "don't populate" field, and materialized-view population/refresh is core's responsibility — reject rather than silently create a populated view:

```rust
// add near the top of translate_create_table_as, after the objtype check (~line 986)
if into_clause.skip_data {
    return Err(ParseError::ParseError(
        "CREATE MATERIALIZED VIEW ... WITH NO DATA is not supported (Turso materialized \
         views are always live/populated)".into(),
    ));
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_create_or_replace_view_emits_drop_then_create -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): CREATE OR REPLACE VIEW emits DROP+CREATE; reject unsupported WITH NO DATA"
```

**Notes:** Real `WITH NO DATA` support needs a Turso-core plan for an "unpopulated matview" state plus a corresponding `REFRESH MATERIALIZED VIEW` to populate it — today `is_refresh_matview` (translator.rs:5117) treats REFRESH as a no-op since Turso matviews are always live; that assumption would need to change too. Not scheduled here.

---

### Task B25: `CREATE INDEX` rejects unsupported `USING`/`INCLUDE`/`NULLS NOT DISTINCT` instead of silently dropping them (M11)

**Files:** Modify: `parser_pg/src/translator.rs:745-807` (`translate_create_index`)

Confirmed via protobuf: `IndexStmt.access_method: String` (default `"btree"`), `IndexStmt.index_including_params: Vec<Node>`, `IndexStmt.nulls_not_distinct: bool` — none read; `translate_create_index` hardcodes `using: None`.

- [ ] **Step 1: Implement**

```rust
// before (line 797-806)
Ok(ast::Stmt::CreateIndex {
    unique: idx.unique,
    if_not_exists: idx.if_not_exists,
    idx_name,
    tbl_name,
    using: None,
    columns,
    with_clause: vec![],
    where_clause,
})

// after
if !idx.access_method.is_empty() && !idx.access_method.eq_ignore_ascii_case("btree") {
    return Err(ParseError::ParseError(format!(
        "CREATE INDEX USING {} is not supported (Turso indexes are always B-tree)",
        idx.access_method
    )));
}
if !idx.index_including_params.is_empty() {
    return Err(ParseError::ParseError(
        "CREATE INDEX ... INCLUDE is not supported".into(),
    ));
}
if idx.nulls_not_distinct {
    return Err(ParseError::ParseError(
        "CREATE UNIQUE INDEX ... NULLS NOT DISTINCT is not supported".into(),
    ));
}

Ok(ast::Stmt::CreateIndex {
    unique: idx.unique,
    if_not_exists: idx.if_not_exists,
    idx_name,
    tbl_name,
    using: None,
    columns,
    with_clause: vec![],
    where_clause,
})
```

- [ ] **Step 2: Run full suite**

Run: `cargo test -p turso_parser_pg`
Expected: PASS. `USING btree` (the default/only supported case) remains a silent no-op match, which is correct since it's Turso's only index type — only non-btree access methods are rejected.

- [ ] **Step 3: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): reject CREATE INDEX USING <non-btree>/INCLUDE/NULLS NOT DISTINCT instead of silently dropping them"
```

---

### Task B26: Fix AUTOINCREMENT misattribution when a SERIAL column and a separate table-level PRIMARY KEY coexist (M12)

**Files:** Modify: `parser_pg/src/translator.rs:184-245` (`translate_create_table`)

**Root cause (confirmed):** `has_autoincrement` is a single table-wide bool set to `true` if *any* column is `SERIAL`, with no record of *which* column. When a table-level `PRIMARY KEY` constraint exists on a *different* (non-serial) column, that table-level PK unconditionally receives `auto_increment: has_autoincrement` — e.g. `CREATE TABLE t (id SERIAL, name TEXT, PRIMARY KEY (name))` incorrectly attaches `AUTOINCREMENT` to `PRIMARY KEY (name)`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn test_serial_column_autoincrement_not_misattributed_to_other_pk() {
    let translator = PostgreSQLTranslator::new();
    let sql = "CREATE TABLE t (id SERIAL, name TEXT, PRIMARY KEY (name))";
    let parsed = crate::parse(sql).unwrap();
    let translated = translator.translate(&parsed).unwrap();
    if let ast::Stmt::CreateTable {
        body: ast::CreateTableBody::ColumnsAndConstraints { constraints, .. },
        ..
    } = translated
    {
        if let ast::TableConstraint::PrimaryKey { auto_increment, columns, .. } =
            &constraints[0].constraint
        {
            assert_eq!(columns[0].expr.to_string(), "name");
            assert!(
                !auto_increment,
                "AUTOINCREMENT must not be attached to a PK on a non-SERIAL column"
            );
        } else {
            panic!("Expected PrimaryKey constraint");
        }
    } else {
        panic!("Expected CreateTable");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p turso_parser_pg test_serial_column_autoincrement_not_misattributed_to_other_pk -- --nocapture`
Expected: FAIL (`auto_increment` is `true`).

- [ ] **Step 3: Implement**

```rust
// before (lines 186-196)
let mut has_autoincrement = false;
for elt in &create.table_elts {
    let Some(ref inner) = elt.node else { continue };
    if let Node::ColumnDef(col_def) = inner {
        let pg_type = extract_type_name(col_def)?;
        if is_serial_type(&pg_type) {
            has_autoincrement = true;
            break;
        }
    }
}

// after
let mut serial_column: Option<String> = None;
for elt in &create.table_elts {
    let Some(ref inner) = elt.node else { continue };
    if let Node::ColumnDef(col_def) = inner {
        let pg_type = extract_type_name(col_def)?;
        if is_serial_type(&pg_type) {
            serial_column = Some(col_def.colname.clone());
            break;
        }
    }
}
let has_autoincrement = serial_column.is_some();
```

```rust
// before (line 227-245, table-level PK constraint)
ConstrType::ConstrPrimary => {
    let pk_cols = extract_key_columns(&constraint.keys)?;
    table_constraints.push(ast::NamedTableConstraint {
        name: None,
        constraint: ast::TableConstraint::PrimaryKey {
            columns: pk_cols.into_iter().map(|c| ast::SortedColumn {
                expr: Box::new(ast::Expr::Id(ast::Name::from_string(c))),
                order: None,
                nulls: None,
            }).collect(),
            auto_increment: has_autoincrement,
            conflict_clause: None,
        },
    });
}

// after
ConstrType::ConstrPrimary => {
    let pk_cols = extract_key_columns(&constraint.keys)?;
    let pk_is_serial_column = pk_cols.len() == 1
        && serial_column.as_deref() == Some(pk_cols[0].as_str());
    table_constraints.push(ast::NamedTableConstraint {
        name: None,
        constraint: ast::TableConstraint::PrimaryKey {
            columns: pk_cols.into_iter().map(|c| ast::SortedColumn {
                expr: Box::new(ast::Expr::Id(ast::Name::from_string(c))),
                order: None,
                nulls: None,
            }).collect(),
            auto_increment: pk_is_serial_column,
            conflict_clause: None,
        },
    });
}
```

`translate_create_table_column`'s column-level PK emission only fires when there's no table-level PK, and passes `has_autoincrement` — extend it to gate on `serial_column == Some(this column's name)` rather than the table-wide bool, for the rarer case where a *different* column is SERIAL while *this* column is the sole column-level PK. This requires threading either the serial column's name or a pre-computed `is_this_column_serial: bool` into `translate_create_table_column`'s existing `has_autoincrement: bool` parameter — a small signature change with two call-site updates.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p turso_parser_pg test_serial_column_autoincrement_not_misattributed_to_other_pk -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add parser_pg/src/translator.rs
git commit -S -m "fix(translator): stop misattributing AUTOINCREMENT to a table-level PK when a different column is SERIAL"
```

---

### Task B27 (verification spike, not a guessed fix): `WITHIN GROUP` ordered-set aggregate semantics unverified (M13)

**Do not implement a fix without verifying the bug first — per the project's debugging protocol ("validate your hypotheses").**

**Files:** `parser_pg/src/translator.rs:3224-3338` (`translate_func_call`)

Confirmed `FuncCall.agg_within_group: bool` is never read. Its companion `agg_order` (the `ORDER BY` list) *is* already translated and passed through as `ast::FunctionCall.order_by`, regardless of whether it came from a normal `agg_order` (e.g. `array_agg(x ORDER BY y)`) or a `WITHIN GROUP (ORDER BY ...)` ordered-set aggregate (e.g. `percentile_cont(0.5) WITHIN GROUP (ORDER BY x)`). The ORDER BY *expression* isn't silently dropped, but the two constructs are semantically different in PG, and Turso's `order_by` field on `FunctionCall` may not have the right execution semantics for ordered-set aggregates if the underlying function (e.g. `percentile_cont` in `extensions/percentile`) expects a different calling convention.

- [ ] **Step 1: Write and run an integration test to determine actual behavior**

```rust
#[test]
fn test_within_group_percentile_cont_produces_correct_result() {
    // Run against a real pgmicro connection with known data, e.g.:
    // CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1),(2),(3),(4),(5);
    // SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM t;
    // Expected PG result: 3. Assert the actual pgmicro result matches.
}
```

Run via `cargo test -p core_tester --test integration_tests integration::postgres` (add this test there) or manually via `cargo run -p pgmicro -- :memory:`.

- [ ] **Step 2: If the result is wrong, escalate to a fix task; if correct, close as verified-no-bug**

If wrong, the likely fix is to special-case `agg_within_group == true` and route the order-by expression as a synthetic leading argument matching `percentile_cont`'s actual calling convention — but that requires knowing the extension's real signature first. Do not guess-fix.

---

## Workstream A — Wire/Protocol

**Worktree:** `wt-wire` · **Primary file:** `cli/pg_server.rs` (2141 LOC) · **Test command:** `cargo test -p pgmicro`

Core-API facts (connection/database relationship, dialect/attach scoping, `close()` semantics, notify-hub ownership) verified via direct reading of `core/lib.rs:1763-1872`, `core/connection.rs:159-349,1706-1758,2306-2315`, `pgmicro/src/main.rs:113-168`.

**Landing order:** C4.1 → C4.2 → C4.3 → C4.4 → C4.5 → C4.6 (C4 is foundational and fixes C5/C6 largely as side effects) → C5 (leak-only remainder) → C6 (verify-only) → H21 → H22.1 → H22.2 → H22.3 (checklist, land incrementally) → H23 → M1 → M2 → M3 (fold into C4.5's cleanup tail).

### Task A1 (C4.1): Add `Connection::database()` accessor

**Files:**
- Modify: `core/connection.rs` (new method in `impl Connection`, adjacent to `get_source_database` at line 2306)

**Interfaces:**
- Produces: `pub fn database(&self) -> &Arc<Database>` — the only way for `cli/pg_server.rs` to get from the bootstrap `Arc<Connection>` to `Arc<Database>` so it can call `.connect()` per socket. Consumed by Task A2.

No dedicated unit test — this is a 3-line `pub(crate)`→`pub` accessor with no new behavior (writing one would be padding, per Rule 2: Simplicity First). Its correctness is exercised transitively by Task A6's integration test, which fails today and only passes once this accessor lets Task A3 open real per-socket connections.

- [ ] **Step 1: Implement**

```rust
// before (core/connection.rs, impl Connection, near line 2306)
pub(crate) fn get_source_database(&self, database_id: usize) -> Arc<Database> {
    ...
}

// after
/// The `Database` this connection was opened from. Lets callers open
/// additional independent connections (e.g. one per accepted network
/// client) via `Database::connect`.
pub fn database(&self) -> &Arc<Database> {
    &self.db
}

pub(crate) fn get_source_database(&self, database_id: usize) -> Arc<Database> {
    ...
}
```

`db: Arc<Database>` is already `pub(crate)` (`core/connection.rs:160`), so this is a pure visibility widening — zero risk to existing callers. This is the one unavoidable `core/` change (per CLAUDE.md "minimize core/ changes" — justified as a 3-line accessor, not new logic).

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p turso_core`
Expected: builds clean.

- [ ] **Step 3: Commit**

```bash
git add core/connection.rs
git commit -S -m "feat(core): add Connection::database() accessor for per-socket connection support"
```

---

### Task A2 (C4.2): Refactor `TursoPgServer` to hold `Arc<Database>` + a post-connect hook instead of one shared `Connection`

**Files:**
- Modify: `cli/pg_server.rs:51-88` (struct + `new`)
- Modify: `pgmicro/src/main.rs:1039-1046` (call site)

**Interfaces:**
- Consumes: Task A1's `Connection::database()`.
- Produces: `self.db: Arc<Database>`, `self.post_connect: PostConnectHook` (type alias `Arc<dyn Fn(&Arc<Connection>) -> anyhow::Result<()> + Send + Sync>`) — consumed by Task A3.

Pure signature refactor, no behavior change yet (the server still only ever calls `.connect()` once, at startup, same as today). Correctness gate: `cargo build -p pgmicro` and the full existing `pgmicro/tests/pgmicro.rs` suite must stay green unmodified after this step.

- [ ] **Step 1: Implement**

```rust
// before (cli/pg_server.rs:51-88)
pub struct TursoPgServer {
    address: String,
    db_file: String,
    conn: Arc<Mutex<Arc<Connection>>>,
    interrupt_count: Arc<AtomicUsize>,
    tls_acceptor: Option<pgwire::tokio::TlsAcceptor>,
    cancel_registry: Arc<TursoCancelRegistry>,
}

impl TursoPgServer {
    pub fn new(
        address: String,
        db_file: String,
        conn: Arc<Connection>,
        interrupt_count: Arc<AtomicUsize>,
        tls_cert: Option<&Path>,
        tls_key: Option<&Path>,
    ) -> anyhow::Result<Self> {
        conn.set_sql_dialect(turso_core::SqlDialect::Postgres);
        let tls_acceptor = match (tls_cert, tls_key) { ... };
        Ok(Self {
            address, db_file,
            conn: Arc::new(Mutex::new(conn)),
            interrupt_count, tls_acceptor,
            cancel_registry: Arc::new(TursoCancelRegistry::default()),
        })
    }
    ...
}

// after
/// Runs on every freshly-`connect()`ed per-socket `Connection` before it
/// serves queries: sets the Postgres dialect and replays whatever schema
/// ATTACHes the CLI bootstrap connection did (see `auto_attach_pg_schemas`
/// in pgmicro/src/main.rs), since neither is inherited from `Database::connect()`.
pub type PostConnectHook = Arc<dyn Fn(&Arc<Connection>) -> anyhow::Result<()> + Send + Sync>;

pub struct TursoPgServer {
    address: String,
    db_file: String,
    db: Arc<turso_core::Database>,
    post_connect: PostConnectHook,
    interrupt_count: Arc<AtomicUsize>,
    tls_acceptor: Option<pgwire::tokio::TlsAcceptor>,
    cancel_registry: Arc<TursoCancelRegistry>,
}

impl TursoPgServer {
    pub fn new(
        address: String,
        db_file: String,
        conn: Arc<Connection>,
        interrupt_count: Arc<AtomicUsize>,
        tls_cert: Option<&Path>,
        tls_key: Option<&Path>,
        post_connect: PostConnectHook,
    ) -> anyhow::Result<Self> {
        conn.set_sql_dialect(turso_core::SqlDialect::Postgres);
        post_connect(&conn)?; // apply to the bootstrap connection too, for consistency
        let db = conn.database().clone();
        let tls_acceptor = match (tls_cert, tls_key) { ... };
        Ok(Self {
            address, db_file, db, post_connect,
            interrupt_count, tls_acceptor,
            cancel_registry: Arc::new(TursoCancelRegistry::default()),
        })
    }
    ...
}
```

```rust
// pgmicro/src/main.rs:1039-1046, before
let server = TursoPgServer::new(
    address.clone(), db_file, conn, interrupt_count,
    opts.tls_cert.as_deref(), opts.tls_key.as_deref(),
)?;

// after
let db_file_for_hook = db_file.clone();
let server = TursoPgServer::new(
    address.clone(), db_file, conn, interrupt_count,
    opts.tls_cert.as_deref(), opts.tls_key.as_deref(),
    Arc::new(move |c: &Arc<turso_core::Connection>| {
        auto_attach_pg_schemas(c, &db_file_for_hook);
        Ok(())
    }),
)?;
```

Before wiring, check `auto_attach_pg_schemas`'s actual signature/return type in `pgmicro/src/main.rs` — if it returns `()` not `Result`, wrap the call accordingly rather than assuming.

- [ ] **Step 2: Run existing suite to confirm no behavior change**

Run: `cargo build -p pgmicro && cargo test -p pgmicro`
Expected: PASS, unchanged from before this task.

- [ ] **Step 3: Commit**

```bash
git add cli/pg_server.rs pgmicro/src/main.rs
git commit -S -m "refactor(wire): hold Arc<Database> + post-connect hook instead of one shared Connection"
```

**Notes:** Keeps `auto_attach_pg_schemas` logic in `pgmicro/src/main.rs` (where it already lives) rather than duplicating directory-scan logic into `cli/pg_server.rs` — `cli` stays generic/shared with `tursodb` per the CLAUDE.md structure notes.

---

### Task A3 (C4.3): Open one `Connection` per accepted socket

**Files:**
- Modify: `cli/pg_server.rs:90-161` (`run_async`)
- Modify: `cli/pg_server.rs:262-268` (`TursoPgHandler` struct — field type change)

**Interfaces:**
- Consumes: `self.db`/`self.post_connect` from Task A2.
- Produces: a per-socket `Arc<Connection>` owned directly (no `Mutex`) by a per-socket `Arc<TursoPgHandler>` — consumed by Task A4 (call-site updates) and Task A5 (cleanup wiring). This is the change Task A6's test actually exercises.

- [ ] **Step 1: Write the failing test**

```rust
// pgmicro/tests/pgmicro.rs — add near the other WIRE_PORT_* constants
const WIRE_PORT_CONN_ISOLATION: u16 = 22432;

/// Extract the transaction-status byte from the final ReadyForQuery ('Z') message.
fn ready_for_query_status(data: &[u8]) -> Option<u8> {
    let mut pos = 0;
    let mut status = None;
    while pos < data.len() {
        let tag = data[pos];
        pos += 1;
        if pos + 4 > data.len() { break; }
        let len = i32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        let body_len = len - 4;
        if pos + body_len > data.len() { break; }
        if tag == b'Z' && body_len == 1 {
            status = Some(data[pos]);
        }
        pos += body_len;
    }
    status
}

/// Extract the first column of the first DataRow ('D') message as UTF-8 text.
fn extract_first_data_row_text(data: &[u8]) -> Option<String> {
    let mut pos = 0;
    while pos < data.len() {
        let tag = data[pos];
        pos += 1;
        if pos + 4 > data.len() { break; }
        let len = i32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        let body_len = len - 4;
        if pos + body_len > data.len() { break; }
        if tag == b'D' {
            let body = &data[pos..pos + body_len];
            if body.len() < 6 { return None; }
            let flen = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
            if flen < 0 { return None; }
            let start = 6;
            let end = start + flen as usize;
            return std::str::from_utf8(&body[start..end]).ok().map(|s| s.to_string());
        }
        pos += body_len;
    }
    None
}

#[test]
fn wire_begin_on_one_client_does_not_affect_another() {
    let port = wire_port(WIRE_PORT_CONN_ISOLATION);
    let mut server = start_pgmicro_server(port);

    let mut client_a = PgTestClient::connect(port);
    let mut client_b = PgTestClient::connect(port);

    client_a.query_raw("CREATE TABLE t (id INTEGER)");

    // Client A starts an explicit transaction and inserts a row, but never commits.
    let tags = client_a.query_command_tags("BEGIN");
    assert!(tags.iter().any(|t| t == "BEGIN"));
    client_a.query_command_tags("INSERT INTO t VALUES (1)");

    // Client B, on its own connection, must not see A's uncommitted insert,
    // and must not be reported as "inside a transaction" itself.
    let response_b = client_b.query_raw("SELECT count(*) FROM t");
    assert_eq!(
        ready_for_query_status(&response_b),
        Some(b'I'),
        "client B must be idle, not inside client A's transaction: {response_b:?}"
    );
    assert_eq!(
        extract_first_data_row_text(&response_b).as_deref(),
        Some("0"),
        "client B must not see client A's uncommitted insert: {response_b:?}"
    );

    client_a.query_command_tags("ROLLBACK");
    server.kill().ok();
    server.wait().ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro wire_begin_on_one_client_does_not_affect_another -- --nocapture`
Expected: FAIL — the shared `Connection` means B's `SELECT count(*)` runs inside A's open transaction and sees the uncommitted row (count=1, not 0).

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:90-161, run_async)
async fn run_async(&self) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&self.address).await?;
    ...
    let notify_hub = self.conn.lock().unwrap().pg_notify_hub();
    let notify_registry = Arc::new(TursoPgNotifyRegistry::new(notify_hub));
    let factory = Arc::new(TursoPgFactory {
        handler: Arc::new(TursoPgHandler {
            conn: self.conn.clone(),
            db_file: self.db_file.clone(),
            query_parser: Arc::new(NoopQueryParser::new()),
            copy_in: Arc::new(Mutex::new(CopyInSession::default())),
            notify_registry: notify_registry.clone(),
        }),
        cancel_registry: self.cancel_registry.clone(),
        notify_registry: notify_registry.clone(),
        notify_delivery_tx: Arc::new(Mutex::new(None)),
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((socket, addr)) => {
                        info!("PostgreSQL client connected from {}", addr);
                        let factory_ref =
                            factory.with_notify_delivery_tx(Arc::new(Mutex::new(None)));
                        let tls = tls_acceptor.clone();
                        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
                        *factory_ref.notify_delivery_tx.lock().unwrap() = Some(notify_tx);
                        tokio::spawn(async move {
                            if let Err(e) = process_socket_with_notify(
                                socket, tls, factory_ref, notify_rx,
                            ).await {
                                error!("Error processing connection from {}: {}", addr, e);
                            }
                        });
                    }
                    Err(e) => error!("Error accepting connection: {}", e),
                }
            }
            _ = tokio::signal::ctrl_c() => { break; }
        }
        ...
    }
    Ok(())
}

// after
async fn run_async(&self) -> anyhow::Result<()> {
    let listener = TcpListener::bind(&self.address).await?;
    ...
    let notify_hub = self.db.pg_notify_hub(); // Database-level, shared correctly (see C6 Notes)
    let notify_registry = Arc::new(TursoPgNotifyRegistry::new(notify_hub));
    let cancel_registry = self.cancel_registry.clone();
    let db = self.db.clone();
    let post_connect = self.post_connect.clone();
    let db_file = self.db_file.clone();

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((socket, addr)) => {
                        info!("PostgreSQL client connected from {}", addr);

                        // One independent Connection per accepted socket — no
                        // shared transaction/dialect/attach state across clients.
                        let per_conn = match db.connect() {
                            Ok(c) => c,
                            Err(e) => {
                                error!("Failed to open connection for {}: {}", addr, e);
                                continue;
                            }
                        };
                        per_conn.set_sql_dialect(turso_core::SqlDialect::Postgres);
                        if let Err(e) = post_connect(&per_conn) {
                            error!("post-connect setup failed for {}: {}", addr, e);
                            continue;
                        }

                        let handler = Arc::new(TursoPgHandler {
                            conn: per_conn,
                            db_file: db_file.clone(),
                            query_parser: Arc::new(NoopQueryParser::new()),
                            copy_in: Arc::new(Mutex::new(CopyInSession::default())),
                            notify_registry: notify_registry.clone(),
                        });
                        let factory_ref = Arc::new(TursoPgFactory {
                            handler,
                            cancel_registry: cancel_registry.clone(),
                            notify_registry: notify_registry.clone(),
                            notify_delivery_tx: Arc::new(Mutex::new(None)),
                        });

                        let tls = tls_acceptor.clone();
                        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
                        *factory_ref.notify_delivery_tx.lock().unwrap() = Some(notify_tx);
                        tokio::spawn(async move {
                            if let Err(e) = process_socket_with_notify(
                                socket, tls, factory_ref, notify_rx,
                            ).await {
                                error!("Error processing connection from {}: {}", addr, e);
                            }
                        });
                    }
                    Err(e) => error!("Error accepting connection: {}", e),
                }
            }
            _ = tokio::signal::ctrl_c() => { break; }
        }
        ...
    }
    Ok(())
}
```

```rust
// cli/pg_server.rs:262-268, before
struct TursoPgHandler {
    conn: Arc<Mutex<Arc<Connection>>>,
    db_file: String,
    query_parser: Arc<NoopQueryParser>,
    copy_in: Arc<Mutex<CopyInSession>>,
    notify_registry: Arc<TursoPgNotifyRegistry>,
}

// after
struct TursoPgHandler {
    conn: Arc<Connection>, // one Connection per socket; no Mutex needed
    db_file: String,
    query_parser: Arc<NoopQueryParser>,
    copy_in: Arc<Mutex<CopyInSession>>,
    notify_registry: Arc<TursoPgNotifyRegistry>,
}
```

Also delete `TursoPgFactory::with_notify_delivery_tx` (`cli/pg_server.rs:349-361`) — its job is now inlined into the accept arm above.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro wire_begin_on_one_client_does_not_affect_another -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): open one Connection per accepted socket instead of sharing one across all clients"
```

**Notes:** `Connection: Send + Sync` is asserted at `core/connection.rs:295` (`assert_send_sync!(Connection)`), so `tokio::spawn`-ing a task that owns an `Arc<Connection>` independent from siblings is exactly the pattern already used in `testing/stress/main.rs:705,799,818,1020`. `db.connect()` can fail (e.g. resource limits) — the `continue` on error means that one client's accept is dropped without crashing the server; confirm this is acceptable (vs. logging + closing the socket explicitly) with whoever reviews.

---

### Task A4 (C4.4): Thread the per-connection `Connection` through all handler methods

**Files:**
- Modify: `cli/pg_server.rs:430,502,551,574,864` (`.lock().unwrap().clone()` → `.clone()`)
- Modify: `cli/pg_server.rs:372-379` (`TursoPgFactory::startup_handler()`)

**Interfaces:**
- Consumes: the field-type change from Task A3.
- Produces: correctly-scoped `Connection` access at every call site, including `TursoStartupHandler` — which is also **C5's routing fix**, for free.

Covered by Task A3's `wire_begin_on_one_client_does_not_affect_another` test (this step is what actually makes `do_query` read the right connection) and by re-running the full existing wire test suite (all must still pass, since query semantics for a single client are unchanged).

- [ ] **Step 1: Implement** (five identical mechanical edits)

```rust
// before (×5: cli/pg_server.rs:430, 502, 551, 574, 864)
let conn = self.conn.lock().unwrap().clone();

// after
let conn = self.conn.clone();
```

```rust
// before (cli/pg_server.rs:372-379)
fn startup_handler(&self) -> Arc<impl StartupHandler> {
    Arc::new(TursoStartupHandler {
        conn: self.handler.conn.lock().unwrap().clone(),
        registry: self.cancel_registry.clone(),
        notify_registry: self.notify_registry.clone(),
        notify_delivery_tx: self.notify_delivery_tx.clone(),
    })
}

// after
fn startup_handler(&self) -> Arc<impl StartupHandler> {
    Arc::new(TursoStartupHandler {
        conn: self.handler.conn.clone(),
        registry: self.cancel_registry.clone(),
        notify_registry: self.notify_registry.clone(),
        notify_delivery_tx: self.notify_delivery_tx.clone(),
    })
}
```

- [ ] **Step 2: Run full suite to verify no regression**

Run: `cargo build -p pgmicro && cargo test -p pgmicro`
Expected: PASS. `cargo build` fails fast at every remaining `.lock()` call site on a now-non-`Mutex` field, so there's no risk of missing one.

- [ ] **Step 3: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): thread per-connection Connection through all handler methods, fixing cancel-request routing"
```

---

### Task A5 (C4.5): Close the per-connection `Connection` and unregister it on disconnect

**Files:**
- Modify: `cli/pg_server.rs:1562-1647` (`process_socket_with_notify` signature + tail)

**Interfaces:**
- Consumes: the per-socket `Arc<Connection>` from Task A3/A4.

Needed because `Connection::close()` (`core/connection.rs:1706-1754`) does WAL/MVCC rollback, checkpoint-on-last-connection, and `pg_listen::unregister_connection` (line 1752) — none of which `Drop` (`core/connection.rs:297-349`) fully replicates (Drop does not call `pg_listen::unregister_connection`).

- [ ] **Step 1: Write the failing test**

```rust
const WIRE_PORT_CONNECT_CHURN: u16 = 27432;

#[test]
fn wire_repeated_connect_disconnect_does_not_leak_or_wedge_server() {
    let port = wire_port(WIRE_PORT_CONNECT_CHURN);
    let mut server = start_pgmicro_server(port);

    for _ in 0..25 {
        let mut c = PgTestClient::connect(port);
        c.query_raw("SELECT 1");
        drop(c); // abrupt disconnect, no explicit Terminate
    }

    // Server must still be responsive after 25 connect/disconnect cycles.
    let mut c = PgTestClient::connect(port);
    let resp = c.query_raw("SELECT 1");
    assert!(!response_has_error(&resp));

    server.kill().ok();
    server.wait().ok();
}
```

This is a coarse smoke test (a tight leak assertion needs a `pg_stat_activity`-style connection-count introspection point that doesn't exist yet); it at least catches a wedged/panicking server from missing cleanup. If `Database::n_connections` (`core/lib.rs:1865-1866`) is ever made `pub`, tighten this test to assert the count returns to 1 (the CLI bootstrap connection) after all wire clients disconnect.

- [ ] **Step 2: Run test to verify it fails or is at least a meaningful gate**

Run: `cargo test -p pgmicro wire_repeated_connect_disconnect_does_not_leak_or_wedge_server -- --nocapture`
Expected: PASS today too (it's a coarse smoke test) but implement the fix regardless since the underlying resource leak (`pg_listen::unregister_connection` never called) is real and silent — this is a Fail Loud gap, not a crash bug, so this particular test may not catch it; the fix still lands.

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:1562-1567, signature)
async fn process_socket_with_notify(
    tcp_socket: tokio::net::TcpStream,
    tls_acceptor: Option<pgwire::tokio::TlsAcceptor>,
    handlers: Arc<TursoPgFactory>,
    mut notify_rx: mpsc::UnboundedReceiver<PgNotification>,
) -> Result<(), std::io::Error> {

// after — capture the handler's Connection before handlers are type-erased
async fn process_socket_with_notify(
    tcp_socket: tokio::net::TcpStream,
    tls_acceptor: Option<pgwire::tokio::TlsAcceptor>,
    handlers: Arc<TursoPgFactory>,
    mut notify_rx: mpsc::UnboundedReceiver<PgNotification>,
) -> Result<(), std::io::Error> {
    let conn_for_cleanup = handlers.handler.conn.clone();
    let cancel_registry_ref = handlers.cancel_registry.clone();
```

```rust
// before (cli/pg_server.rs:1645-1647, tail)
    notify_registry.unregister_wire_session(socket.pid_and_secret_key().0);
    Ok(())
}

// after
    let (pid, secret) = socket.pid_and_secret_key();
    notify_registry.unregister_wire_session(pid);
    cancel_registry_ref.unregister(pid, secret); // see Task A7 (C5)
    if let Err(e) = conn_for_cleanup.close() {
        tracing::warn!("connection cleanup failed: {}", e);
    }
    Ok(())
}
```

- [ ] **Step 4: Run full suite**

Run: `cargo test -p pgmicro`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): close per-connection Connection and unregister cancel/notify state on disconnect"
```

**Notes:** `cancel_registry_ref.unregister` requires Task A7 (C5)'s `TursoCancelRegistry::unregister` method — land Task A5 and Task A7 together if working strictly in landing order, or stub `unregister` here and let Task A7 fill it in; either sequencing compiles cleanly since Rust doesn't care about task numbering, only about the method existing before this call site references it.

---

### Task A6 (C4.6): Full isolation test coverage (dialect/PRAGMA + transaction, two clients)

**Files:**
- Test: `pgmicro/tests/pgmicro.rs`

**Interfaces:**
- Final acceptance check for the whole C4 effort; consumes Tasks A1-A5.

The `wire_begin_on_one_client_does_not_affect_another` test from Task A3 is the primary one. Add this second test for the dialect-isolation angle specifically (a distinct code path from transaction state, since `sql_dialect` is a separate `AtomicSqlDialect`):

- [ ] **Step 1: Write the test**

```rust
const WIRE_PORT_DIALECT_ISOLATION: u16 = 28432;

#[test]
fn wire_pragma_on_one_client_does_not_affect_another() {
    let port = wire_port(WIRE_PORT_DIALECT_ISOLATION);
    let mut server = start_pgmicro_server(port);

    let mut client_a = PgTestClient::connect(port);
    let mut client_b = PgTestClient::connect(port);

    // Client A flips to sqlite dialect on its own connection.
    let tags = client_a.query_command_tags("SET sql_dialect = 'sqlite'");
    assert!(!tags.is_empty());

    // Client B must still be running in postgres dialect — e.g. PG-only
    // syntax like RETURNING must still work for B.
    client_b.query_raw("CREATE TABLE t (id INTEGER)");
    let resp = client_b.query_raw("INSERT INTO t (id) VALUES (1) RETURNING id");
    assert!(
        !response_has_error(&resp),
        "client B lost postgres dialect due to client A's SET: {resp:?}"
    );

    server.kill().ok();
    server.wait().ok();
}
```

Before landing, confirm `SET sql_dialect = 'sqlite'` is the correct wire-visible syntax (per CLAUDE.md, `SET`→`PRAGMA` rewrite happens in `try_prepare_pg()`, `core/connection.rs`) against existing `PRAGMA sql_dialect` docs/tests rather than assuming.

- [ ] **Step 2: Run both isolation tests against the completed A1-A5 stack**

Run: `cargo test -p pgmicro wire_begin_on_one_client_does_not_affect_another wire_pragma_on_one_client_does_not_affect_another -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add pgmicro/tests/pgmicro.rs
git commit -S -m "test(wire): add dialect-isolation coverage for per-connection state (C4 acceptance)"
```

---

### Task A7 (C5): Cancellation must target only the requesting client's connection

**Files:**
- Modify: `cli/pg_server.rs:163-172` (`TursoCancelRegistry`)
- Modify: `cli/pg_server.rs:1562-1647` (cleanup tail, ties into Task A5)

**Interfaces:**
- Consumes: Task A4 (which already fixes *registration* to use the correct per-connection `Connection`).
- Produces: `unregister()` used by Task A5.

The *routing* half of C5 (cancel hitting the wrong connection) is fixed entirely by Task A4 — once `TursoStartupHandler::post_startup` (`cli/pg_server.rs:228-253`) registers `self.handler.conn.clone()` and that field is per-socket, `TursoCancelHandler::on_cancel_request` (`cli/pg_server.rs:205-217`, unchanged) already does `conn.interrupt()` on the correct, now-isolated `Connection`. C5's own remaining work is purely the **leak fix** (entries never removed from `sessions` today, regardless of C4) plus a regression test proving isolation end-to-end.

- [ ] **Step 1: Write the failing test**

```rust
use std::io::Write as _; // already imported in this test file

/// Extract (pid, secret_key) from a BackendKeyData ('K') message.
fn extract_backend_key_data(data: &[u8]) -> Option<(i32, i32)> {
    let mut pos = 0;
    while pos < data.len() {
        let tag = data[pos];
        pos += 1;
        if pos + 4 > data.len() { break; }
        let len = i32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]) as usize;
        pos += 4;
        let body_len = len - 4;
        if pos + body_len > data.len() { break; }
        if tag == b'K' && body_len == 8 {
            let pid = i32::from_be_bytes([data[pos], data[pos+1], data[pos+2], data[pos+3]]);
            let secret = i32::from_be_bytes([data[pos+4], data[pos+5], data[pos+6], data[pos+7]]);
            return Some((pid, secret));
        }
        pos += body_len;
    }
    None
}

fn send_cancel_request(port: u16, pid: i32, secret_key: i32) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    let mut buf = Vec::new();
    buf.extend_from_slice(&16i32.to_be_bytes());
    buf.extend_from_slice(&80877102i32.to_be_bytes()); // CancelRequest code
    buf.extend_from_slice(&pid.to_be_bytes());
    buf.extend_from_slice(&secret_key.to_be_bytes());
    stream.write_all(&buf).unwrap();
    stream.flush().unwrap();
}

const WIRE_PORT_CANCEL_TARGET: u16 = 26432;

#[test]
fn wire_cancel_request_only_interrupts_targeted_connection() {
    let port = wire_port(WIRE_PORT_CANCEL_TARGET);
    let mut server = start_pgmicro_server(port);

    let mut victim = PgTestClient::connect(port);
    let mut bystander = PgTestClient::connect(port);
    let (victim_pid, victim_secret) = extract_backend_key_data(&victim.read_until_ready_startup_bytes())
        .expect("victim BackendKeyData");
    // NOTE: PgTestClient::connect must be extended to retain the raw startup
    // response bytes (or pid/secret fields directly) for this extraction —
    // see Notes.

    victim.query_raw("CREATE TABLE t (id INTEGER)");
    victim.send_query(
        "WITH RECURSIVE slow(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM slow WHERE n < 50000000) \
         SELECT count(*) FROM slow",
    );

    std::thread::sleep(std::time::Duration::from_millis(200));
    send_cancel_request(port, victim_pid, victim_secret);

    let victim_resp = victim.read_until_ready();
    assert!(response_has_error(&victim_resp), "victim query should be cancelled: {victim_resp:?}");

    let bystander_resp = bystander.query_raw("SELECT 1");
    assert!(
        !response_has_error(&bystander_resp),
        "bystander must be unaffected by a cancel targeted at another connection: {bystander_resp:?}"
    );

    server.kill().ok();
    server.wait().ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro wire_cancel_request_only_interrupts_targeted_connection -- --nocapture`
Expected: FAIL until `PgTestClient` gains the pid/secret capture extension described in Notes, and until Task A4 has landed (before that, both clients share one connection, making the isolation assertion meaningless).

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:163-172)
#[derive(Default)]
struct TursoCancelRegistry {
    sessions: Mutex<HashMap<(i32, SecretKey), Arc<Connection>>>,
}

impl TursoCancelRegistry {
    fn register(&self, pid: i32, secret: SecretKey, conn: Arc<Connection>) {
        self.sessions.lock().unwrap().insert((pid, secret), conn);
    }
}

// after
#[derive(Default)]
struct TursoCancelRegistry {
    sessions: Mutex<HashMap<(i32, SecretKey), Arc<Connection>>>,
}

impl TursoCancelRegistry {
    fn register(&self, pid: i32, secret: SecretKey, conn: Arc<Connection>) {
        self.sessions.lock().unwrap().insert((pid, secret), conn);
    }

    fn unregister(&self, pid: i32, secret: SecretKey) {
        self.sessions.lock().unwrap().remove(&(pid, secret));
    }
}
```

Cleanup wiring lands as part of Task A5's tail-of-`process_socket_with_notify` edit (`cancel_registry_ref.unregister(pid, secret);`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro wire_cancel_request_only_interrupts_targeted_connection -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): remove cancel-registry entries on disconnect to stop the session-map leak"
```

**Notes:** `PgTestClient` needs a small extension (store `pid`/`secret_key` fields, populated during `connect()` from the BackendKeyData message) — `read_until_ready_startup_bytes()` in the test above is a placeholder name; wire it to whatever the actual capture mechanism ends up being on `PgTestClient`. Test has inherent timing sensitivity (`sleep(200ms)` before cancelling) — flag as a known flakiness risk; if CI shows flakes, consider a test-only hook to signal "query has started" deterministically instead of a fixed sleep.

---

### Task A8 (C6): Verify COPY buffer isolation across clients (no production fix expected)

**Files:**
- Modify: `cli/pg_server.rs:266` (field, unchanged post-Task A3), `:833,441,507,845` (usage sites, unchanged)
- Test: `pgmicro/tests/pgmicro.rs`

**Interfaces:**
- Consumes: Task A3 (once `TursoPgHandler` — and therefore its `copy_in` field — is constructed fresh per accepted socket instead of once for the whole server, this bug is fixed as a side effect, with zero code changes to the COPY logic itself).

- [ ] **Step 1: Write the test**

```rust
const WIRE_PORT_COPY_ISOLATION: u16 = 25432;

#[test]
fn wire_concurrent_copy_sessions_do_not_cross_talk() {
    let port = wire_port(WIRE_PORT_COPY_ISOLATION);
    let mut server = start_pgmicro_server(port);

    let mut client_a = PgTestClient::connect(port);
    let mut client_b = PgTestClient::connect(port);

    client_a.query_raw("CREATE TABLE t1 (id INTEGER, name TEXT)");
    client_a.query_raw("CREATE TABLE t2 (id INTEGER, name TEXT)");

    // Client A starts COPY into t1 but does not finish it yet.
    client_a.send_query("COPY t1 FROM STDIN");
    let (tag_a, _) = client_a.read_one_message();
    assert_eq!(tag_a, b'G', "expected CopyInResponse for client A");

    // Client B starts (and completes) its own COPY into t2 while A's copy
    // session is still open — with a shared copy_in buffer this clobbers A.
    let tags_b = client_b.copy_from_stdin("COPY t2 FROM STDIN", "9\tZed\n");
    assert_eq!(tags_b, vec!["COPY 1"]);

    // Client A now finishes its own copy.
    client_a.send_frontend_message(b'd', b"1\tAlice\n");
    client_a.send_frontend_message(b'c', &[]);
    let mut tags_a = Vec::new();
    loop {
        let (tag, body) = client_a.read_one_message();
        match tag {
            b'C' => tags_a.push(String::from_utf8_lossy(&body).trim_end_matches('\0').to_string()),
            b'Z' => break,
            _ => {}
        }
    }
    assert_eq!(tags_a, vec!["COPY 1"]);

    // Each row must have landed in the table it was addressed to.
    let resp = client_a.query_raw("SELECT name FROM t1");
    assert_eq!(extract_first_data_row_text(&resp).as_deref(), Some("Alice"));
    let resp = client_a.query_raw("SELECT name FROM t2");
    assert_eq!(extract_first_data_row_text(&resp).as_deref(), Some("Zed"));

    server.kill().ok();
    server.wait().ok();
}
```

This is a deterministic reproduction (not a timing race) of the exact clobbering bug: B's `COPY ... FROM STDIN` while A's session is still open resets the shared buffer today, so `tags_a` or the final table contents come out wrong before Task A3 lands.

- [ ] **Step 2: Run test against the completed A1-A3 stack**

Run: `cargo test -p pgmicro wire_concurrent_copy_sessions_do_not_cross_talk -- --nocapture`
Expected: PASS with **no changes** to lines 266/441/507/833/845 at all — explicitly a "verify, don't touch" task. It exists purely to catch a regression if a future change reintroduces a server-wide handler singleton.

- [ ] **Step 3: Commit**

```bash
git add pgmicro/tests/pgmicro.rs
git commit -S -m "test(wire): add regression coverage for per-connection COPY buffer isolation"
```

**Notes:** Optional follow-up (not required to close C6): since `copy_in` is now single-owner, `Arc<Mutex<CopyInSession>>` could be simplified — but `CopyHandler`'s trait methods take `&self` on a type stored as `Arc<impl CopyHandler>` shared with the `PgWireServerHandlers` factory pattern, so interior mutability is still required. Leave as `Mutex`; do not introduce `RefCell` without checking `CopyHandler`'s trait bounds first.

---

### Task A9 (H21): BEGIN/COMMIT/ROLLBACK must emit `Response::TransactionStart`/`TransactionEnd`

**Files:**
- Modify: `cli/pg_server.rs:961-969` (`execute_non_query`)
- Test: `pgmicro/tests/pgmicro.rs`

**Interfaces:**
- Standalone — independent of C4 (though correctness of *isolation* between clients' status bytes depends on C4 too; the status-byte-classification bug is real and fixable regardless).

- [ ] **Step 1: Write the failing test**

```rust
const WIRE_PORT_TXN_STATUS: u16 = 23432;

#[test]
fn wire_ready_for_query_reports_transaction_status() {
    let port = wire_port(WIRE_PORT_TXN_STATUS);
    let mut server = start_pgmicro_server(port);
    let mut client = PgTestClient::connect(port);

    client.query_raw("CREATE TABLE t (id INTEGER UNIQUE)");

    // Idle before BEGIN.
    let resp = client.query_raw("SELECT 1");
    assert_eq!(ready_for_query_status(&resp), Some(b'I'));

    // In transaction after BEGIN.
    let resp = client.query_raw("BEGIN");
    assert_eq!(ready_for_query_status(&resp), Some(b'T'));

    // Still in transaction after a successful statement inside it.
    let resp = client.query_raw("INSERT INTO t VALUES (1)");
    assert_eq!(ready_for_query_status(&resp), Some(b'T'));

    // A failing statement inside the transaction aborts it (PG semantics).
    let resp = client.query_raw("INSERT INTO t VALUES (1)"); // UNIQUE violation
    assert!(response_has_error(&resp));
    assert_eq!(ready_for_query_status(&resp), Some(b'E'));

    // Idle again after ROLLBACK.
    let resp = client.query_raw("ROLLBACK");
    assert_eq!(ready_for_query_status(&resp), Some(b'I'));

    server.kill().ok();
    server.wait().ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro wire_ready_for_query_reports_transaction_status -- --nocapture`
Expected: FAIL at the `BEGIN` assertion — status stays `'I'` (see `pgwire::api::transaction::TransactionStatus::to_in_transaction_state`/`to_error_state`, never invoked because `execute_non_query` never returns `TransactionStart`/`TransactionEnd`).

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:961-969)
/// Execute a non-SELECT statement and build an Execution response.
fn execute_non_query(stmt: &mut turso_core::Statement, query: &str) -> PgWireResult<Response> {
    stmt.run_ignore_rows()
        .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

    let affected = stmt.n_change();
    let tag = command_tag(query, affected as usize);
    Ok(Response::Execution(tag))
}

// after
/// Execute a non-SELECT statement and build an Execution/TransactionStart/
/// TransactionEnd response. The variant matters: pgwire only updates the
/// ReadyForQuery transaction-status byte ('I'/'T'/'E') when it sees
/// TransactionStart/TransactionEnd, not a plain Execution (see
/// pgwire::api::query::_on_query / _on_execute).
fn execute_non_query(stmt: &mut turso_core::Statement, query: &str) -> PgWireResult<Response> {
    stmt.run_ignore_rows()
        .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

    let affected = stmt.n_change();
    let tag = command_tag(query, affected as usize);
    Ok(classify_transaction_response(query, tag))
}

/// Classify a non-query response as a transaction boundary if the statement
/// starts or ends an explicit transaction block.
fn classify_transaction_response(query: &str, tag: Tag) -> Response {
    let upper = query.trim().to_uppercase();
    if upper.starts_with("BEGIN") || upper.starts_with("START TRANSACTION") {
        Response::TransactionStart(tag)
    } else if upper.starts_with("COMMIT") || upper.starts_with("END") {
        Response::TransactionEnd(tag)
    } else if upper.starts_with("ROLLBACK") && !upper.contains(" TO ") {
        // Plain ROLLBACK / ROLLBACK TRANSACTION ends the block;
        // ROLLBACK TO SAVEPOINT does not.
        Response::TransactionEnd(tag)
    } else {
        Response::Execution(tag)
    }
}
```

`Response` and `Tag` are already imported at the top of the file (`cli/pg_server.rs:37`), no new imports needed.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro wire_ready_for_query_reports_transaction_status -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(wire): emit TransactionStart/TransactionEnd for BEGIN/COMMIT/ROLLBACK so status byte is correct"
```

**Notes:** No fix needed for the "failed statement inside a transaction" half of H21 beyond this — once `BEGIN` correctly reports `'T'`, pgwire's own `TransactionStatus::to_error_state()` handles the rest automatically via the existing `Response::Error`/`process_error` path.

---

### Task A10 (H22.1): NUMERIC binary result-format encoding

**Files:**
- Modify: `core/numeric/decimal.rs` (new encoder, mirroring `pg_wire_numeric_binary_to_text` at line 217 and `pg_wire_binary_to_bigdecimal` at line 259)
- Modify: `cli/pg_server.rs:1386-1395,1403-1412`

**Interfaces:**
- Standalone. Establishes the `format: FieldFormat` threading pattern Task A11/A12 reuse.

**Root cause:** `encode_field_with_type_and_format` hard-codes `FieldFormat::Text` for NUMERIC/arrays (`cli/pg_server.rs:1386-1395,1403-1412,1433-1445`); everything else stored as `Value::Text` falls through to `encoder.encode_field(&text)` (`:1446-1450`), which calls `<&str as ToSql>::to_sql(ty, buf)` directly (not `to_sql_checked`, so `accepts()` is never consulted — confirmed in `postgres-types-0.2.9/src/lib.rs:1097-1116`), silently writing raw text bytes for a claimed-binary column of any type.

- [ ] **Step 1: Write the failing test**

```rust
// pgmicro/tests/pgmicro.rs
const WIRE_PORT_BINARY_NUMERIC: u16 = 29432;

#[test]
fn wire_binary_result_format_encodes_numeric_correctly() {
    let port = wire_port(WIRE_PORT_BINARY_NUMERIC);
    let mut server = start_pgmicro_server(port);
    let mut client = PgTestClient::connect(port);

    client.query_raw("CREATE TABLE t (n NUMERIC(10,2))");
    client.query_raw("INSERT INTO t VALUES (42.50)");

    // Parse + Bind requesting BINARY result format for the NUMERIC column,
    // Execute, Sync — reuse execute_prepared machinery but request binary
    // result format (extend execute_prepared or add a variant that sets the
    // result-format-code list to [1] instead of [0]).
    client.execute_prepared_binary_result("s1", "p1", "SELECT n FROM t", &[]);

    let (tag, body) = client.read_one_message(); // expect ParseComplete 1
    assert_eq!(tag, b'1');
    let (tag, _) = client.read_one_message(); // BindComplete
    assert_eq!(tag, b'2');
    let (tag, body) = client.read_one_message(); // DataRow
    assert_eq!(tag, b'D');

    // DataRow: i16 num_fields, then [i32 len, bytes] per field.
    let num_fields = i16::from_be_bytes([body[0], body[1]]);
    assert_eq!(num_fields, 1);
    let flen = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
    let field_bytes = &body[6..6 + flen];

    // A real PG binary numeric payload is NEVER valid UTF-8 text matching
    // "42.50" — it's ndigits(u16) + weight(i16) + sign(u16) + dscale(i16) +
    // ndigits * u16 base-10000 digits. Assert it decodes via the existing
    // decoder rather than being the literal ASCII text "42.50".
    assert_ne!(field_bytes, b"42.50", "NUMERIC binary result must not be raw text bytes");
    let decoded = turso_core::pg_wire_numeric_binary_to_text(field_bytes).unwrap();
    assert_eq!(decoded, "42.50");

    server.kill().ok();
    server.wait().ok();
}
```

(Requires adding a small `execute_prepared_binary_result` helper to `PgTestClient` — same shape as `execute_prepared_binary_int4` at line 1076 but setting the *result*-format-code list in the Bind message instead of the parameter-format list, and taking no bind parameters.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro wire_binary_result_format_encodes_numeric_correctly -- --nocapture`
Expected: FAIL — `field_bytes` equals the literal ASCII text `"42.50"` today.

- [ ] **Step 3: Implement**

```rust
// core/numeric/decimal.rs — new function, near pg_wire_numeric_binary_to_text (line 217)
/// Encode a NUMERIC text representation into PostgreSQL's wire binary format:
/// ndigits(u16) + weight(i16) + sign(u16) + dscale(i16) + ndigits * u16
/// base-10000 digit groups. Mirrors pg_wire_binary_to_bigdecimal in reverse.
#[cfg(feature = "cli_only")]
pub fn pg_wire_numeric_text_to_binary(text: &str) -> crate::Result<Vec<u8>> {
    use num_bigint::Sign;
    let bd: BigDecimal = text
        .parse()
        .map_err(|e| LimboError::Constraint(format!("invalid numeric text: {e}")))?;
    let (bigint, exponent) = bd.as_bigint_and_exponent(); // exponent = -scale
    let dscale = (-exponent).max(0) as i16;
    let (sign, mag) = bigint.to_u32_digits();
    let sign_flag: u16 = if sign == Sign::Minus { 0x4000 } else { 0x0000 };

    // Convert the absolute value to base-10000 groups, most-significant first.
    let digits_str = bigint.magnitude().to_string();
    // Pad so the decimal point falls on a 4-digit boundary, matching PG's
    // "weight" semantics (weight = index of the most-significant base-10000
    // group relative to the decimal point).
    let (int_part_len, groups) = base10000_groups(&digits_str, exponent);
    let weight = (int_part_len as i32 - 1) / 4;

    let mut out = Vec::with_capacity(8 + groups.len() * 2);
    out.extend_from_slice(&(groups.len() as u16).to_be_bytes());
    out.extend_from_slice(&(weight as i16).to_be_bytes());
    out.extend_from_slice(&sign_flag.to_be_bytes());
    out.extend_from_slice(&dscale.to_be_bytes());
    for g in groups {
        out.extend_from_slice(&g.to_be_bytes());
    }
    Ok(out)
}
// base10000_groups: split a decimal digit string (with the given base-10
// exponent) into big-endian u16 groups of 4 decimal digits each, padding to
// group boundaries — implement mirroring the digit-accumulation loop in
// pg_wire_binary_to_bigdecimal (lines 293-296) run in reverse.
```

```rust
// cli/pg_server.rs:1386-1395, before
} else if *pg_type == Type::NUMERIC {
    let text = turso_core::value_to_pg_numeric_text(val)?;
    encoder
        .encode_field_with_type_and_format(
            &text.as_str(), pg_type, FieldFormat::Text, &FormatOptions::default(),
        )
        .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
}

// after
} else if *pg_type == Type::NUMERIC {
    encode_numeric_field(encoder, val, format)?
}
// (identical change at the Numeric::Float arm, lines 1403-1412)

// new helper, near encode_value
fn encode_numeric_field(
    encoder: &mut DataRowEncoder,
    val: &Value,
    format: FieldFormat,
) -> turso_core::Result<()> {
    let text = turso_core::value_to_pg_numeric_text(val)?;
    if format == FieldFormat::Binary {
        let bin = turso_core::pg_wire_numeric_text_to_binary(&text)?;
        encoder
            .encode_field_with_type_and_format(&bin.as_slice(), &Type::NUMERIC, FieldFormat::Binary, &FormatOptions::default())
    } else {
        encoder
            .encode_field_with_type_and_format(&text.as_str(), &Type::NUMERIC, FieldFormat::Text, &FormatOptions::default())
    }
    .map_err(|e| turso_core::LimboError::InternalError(e.to_string()))
}
```

`DataRowEncoder` has no public "what format was requested for this column" query (confirmed: `encode_field_with_type_and_format` ignores schema-provided format information, `postgres-types` `src/api/results.rs:227-230`) — `encode_value`'s caller (`encode_row`, `cli/pg_server.rs:892-904`) already has `header: &Arc<Vec<FieldInfo>>` with the real per-column format via `FieldInfo::format()`; thread that field-format value into `encode_value`/`encode_numeric_field` as an explicit `format: FieldFormat` parameter passed down from `encode_row`, rather than trying to query it off the encoder. `&[u8]` (the `bin` payload) needs a `ToSql`/`ToSqlText` impl usable with `encode_field_with_type_and_format`; `postgres-types` provides one for `&[u8]` used already for BYTEA (`cli/pg_server.rs:1452-1454`), so this is a drop-in.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro wire_binary_result_format_encodes_numeric_correctly -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/numeric/decimal.rs cli/pg_server.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(wire): NUMERIC binary result format now encodes real binary payload instead of raw text bytes"
```

---

### Task A11 (H22.2): DATE binary encoding (representative `Value::Text`-backed type)

**Files:**
- Modify: `cli/pg_server.rs:1361-1456` (`encode_value`, `Value::Text` branch), reusing `days_from_civil`/`civil_from_days`/`format_pg_date` already defined at `:1057-1094`

**Interfaces:**
- Consumes: the `format: FieldFormat` parameter threaded through in Task A10.
- Produces: `fn try_encode_text_value_binary(text: &str, pg_type: &Type) -> Option<Vec<u8>>` — establishes the pattern Task A12 generalizes to the remaining `Value::Text`-backed types.

- [ ] **Step 1: Write the failing test**

```rust
const WIRE_PORT_BINARY_DATE: u16 = 30432;

#[test]
fn wire_binary_result_format_encodes_date_correctly() {
    let port = wire_port(WIRE_PORT_BINARY_DATE);
    let mut server = start_pgmicro_server(port);
    let mut client = PgTestClient::connect(port);

    client.query_raw("CREATE TABLE t (d DATE)");
    client.query_raw("INSERT INTO t VALUES ('2024-03-15')");

    client.execute_prepared_binary_result("s1", "p1", "SELECT d FROM t", &[]);
    let _ = client.read_one_message(); // ParseComplete
    let _ = client.read_one_message(); // BindComplete
    let (tag, body) = client.read_one_message(); // DataRow
    assert_eq!(tag, b'D');

    let flen = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
    assert_eq!(flen, 4, "binary DATE must be exactly 4 bytes (i32 days since 2000-01-01)");
    let field_bytes = &body[6..6 + flen];
    let days = i32::from_be_bytes(field_bytes.try_into().unwrap());
    // 2024-03-15 is 8840 days after 2000-01-01.
    assert_eq!(days, 8840);

    server.kill().ok();
    server.wait().ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro wire_binary_result_format_encodes_date_correctly -- --nocapture`
Expected: FAIL — `flen` reflects the raw text length of `"2024-03-15"` (10 bytes), not 4.

- [ ] **Step 3: Implement**

```rust
// cli/pg_server.rs:1419-1451, before (relevant excerpt of the Value::Text branch)
Value::Text(t) => {
    let text = t.value.as_ref();
    if *pg_type == Type::TIMESTAMPTZ && !text.contains('+') && !text.contains('Z') && !text.ends_with("-00") {
        let with_tz = format!("{text}+00");
        encoder.encode_field(&with_tz.as_str()) ...
    } else if pg_type.name().starts_with('_') {
        encoder.encode_field_with_type_and_format(&text, &Type::TEXT, FieldFormat::Text, &FormatOptions::default()) ...
    } else {
        encoder.encode_field(&text) ...
    }
}

// after — add a binary-format short-circuit before the existing text logic
Value::Text(t) => {
    let text = t.value.as_ref();
    if format == FieldFormat::Binary {
        if let Some(bin) = try_encode_text_value_binary(text, pg_type) {
            return encoder
                .encode_field_with_type_and_format(&bin.as_slice(), pg_type, FieldFormat::Binary, &FormatOptions::default())
                .map_err(|e| turso_core::LimboError::InternalError(e.to_string()));
        }
        // fall through to text encoding for types without a binary encoder yet (Task A12)
    }
    if *pg_type == Type::TIMESTAMPTZ && !text.contains('+') && !text.contains('Z') && !text.ends_with("-00") {
        ...
    } else if pg_type.name().starts_with('_') {
        ...
    } else {
        encoder.encode_field(&text) ...
    }
}

/// Encode a PG-text-formatted Value::Text into binary wire format for the
/// type families that have an encoder implemented. Returns None for types
/// not yet covered (falls back to text encoding — Task A12 tracks the rest).
fn try_encode_text_value_binary(text: &str, pg_type: &Type) -> Option<Vec<u8>> {
    match *pg_type {
        Type::DATE => {
            let (y, m, d) = parse_iso_date(text)?; // new small parser, mirrors format_pg_date's inverse
            let days = days_from_civil(y, m, d) - days_from_civil(2000, 1, 1);
            Some(days.to_be_bytes().to_vec())
        }
        _ => None,
    }
}
```

`days_from_civil`/`civil_from_days` already exist (`cli/pg_server.rs:1057-1089`) and are exactly what's needed in reverse for DATE — only a small `parse_iso_date(text: &str) -> Option<(i32,u32,u32)>` needs writing (split on `-`, parse three integers; check `format_pg_date`'s output range first for whether BC-era suffixes ever appear).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro wire_binary_result_format_encodes_date_correctly -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): DATE binary result format now encodes real i32-days payload instead of raw text bytes"
```

---

### Task A12 (H22.3): Remaining binary encoders — pattern checklist, land incrementally

**Files:**
- Modify: `try_encode_text_value_binary` (added in Task A11), extend its `match`
- Modify: `cli/pg_server.rs:1433-1445` (remove the array force-to-text once array binary encoding lands)

**Interfaces:**
- Consumes: Task A11's `try_encode_text_value_binary` scaffold.

One parametrized-by-type test per remaining family, same shape as Task A11's test (Parse/Bind-binary/Execute, assert `flen` and decoded bytes match the documented PG binary layout, and assert the bytes are *not* equal to the ASCII text). Do not write all of them at once — land one type family at a time, each as its own S-sized sub-task following this checklist:

- **TIME/TIMESTAMP/TIMESTAMPTZ**: `i64` microseconds — parse existing text output (`format_pg_time_micros`/`format_pg_timestamp_micros`, already reversible) and reuse `read_be_i64`'s counterpart encoding.
- **UUID**: 16 raw bytes — parse the `8-4-4-4-12` hex text (inverse of `format_uuid`, `:1123-1137`).
- **INTERVAL**: 16 bytes (`i64` micros + `i32` days + `i32` months) — inverse of `format_pg_interval_binary` (`:1139-1168`); note this requires parsing PG's `"N mons M days S.ffffff secs"` text back apart, which is lossier/harder than the others — do this one last.
- **MONEY**: `i64` cents — inverse of `format_money_cents` (`:1170-1180`).
- **Arrays** (`pg_type.name().starts_with('_')`, `:1433-1445`): materially bigger — PG binary array format is `ndim(i32), flags(i32), elem_oid(i32), [dim_size(i32), lower_bound(i32)] * ndim, then per-element [len(i32), bytes]`. Since pgmicro stores arrays as pre-formatted text literals (`"{1,2,3}"`) in `Value::Text`, this requires parsing that literal into elements first, then binary-encoding each element with its own scalar encoder (recursion into this same `try_encode_text_value_binary` dispatch) — scope as its own **L**-sized follow-on task, not bundled with the scalar types above.
- **JSON/JSONB**: JSONB binary additionally needs a leading `0x01` version byte before the text payload; JSON binary is identical to text. Small, do together.
- **INET/CIDR/MACADDR/MACADDR8**: PG binary INET/CIDR format has its own 4-byte header (family, prefix length, is_cidr flag, address length) before the raw address bytes — needs its own parser of the stored text form; scope separately.

- [ ] **Step 1: For each family above, write its test first (see Task A11's test as the template), verify it fails, implement the encoder arm, verify it passes, commit** — repeat per family, one commit per family:

```bash
git add cli/pg_server.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(wire): add binary result-format encoder for <FAMILY>"
```

**Notes:** Until each family lands, its binary-format request still silently falls back to text bytes (the Task A11 scaffold's `None` branch) — this is a **partial** fix, not a full close-out of H22. Track the remaining families as an explicit checklist/tracking issue so "binary format support" isn't silently claimed complete after only NUMERIC+DATE land.

---

### Task A13 (H23): Multi-statement simple-query batches must not drop completed statements' responses on a later error

**Files:**
- Modify: `cli/pg_server.rs:420-478` (`SimpleQueryHandler::do_query`)

**Interfaces:**
- Standalone.

- [ ] **Step 1: Write the failing test**

```rust
const WIRE_PORT_BATCH_ERROR: u16 = 24432;

#[test]
fn wire_simple_query_batch_reports_completed_statements_before_error() {
    let port = wire_port(WIRE_PORT_BATCH_ERROR);
    let mut server = start_pgmicro_server(port);
    let mut client = PgTestClient::connect(port);

    client.query_raw("CREATE TABLE t (id INTEGER UNIQUE)");

    let resp = client.query_raw(
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (1);",
    );
    let tags = extract_command_tags(&resp);
    assert_eq!(
        tags,
        vec!["INSERT 0 1", "INSERT 0 1"],
        "expected CommandComplete for both successful statements before the failing one: {resp:?}"
    );
    assert!(response_has_error(&resp), "expected an ErrorResponse for the duplicate insert");

    // Confirm the two successful inserts actually landed (side effects
    // happened even though the batch as a whole reported an error).
    let resp = client.query_raw("SELECT count(*) FROM t");
    assert_eq!(extract_first_data_row_text(&resp).as_deref(), Some("2"));

    server.kill().ok();
    server.wait().ok();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro wire_simple_query_batch_reports_completed_statements_before_error -- --nocapture`
Expected: FAIL — `extract_command_tags(&resp)` returns `[]` because `do_query` propagates the third statement's error via `?`, so the framework's `_on_query` never receives a `Vec<Response>` at all — the client only sees the `ErrorResponse`.

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:420-478)
async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
where ... {
    flush_pending_wire_notifications(&self.notify_registry, client).await?;
    let conn = self.conn.clone();

    let statements = turso_parser_pg::split_statements(query)
        .map_err(|e| PgWireError::UserError(Box::new(parse_error_info(&e.to_string()))))?;

    let mut responses = Vec::new();
    for sql in &statements {
        if let Some(copy) = parse_copy_stdin(sql) {
            let cols = copy_column_count(&conn, &copy)?;
            *self.copy_in.lock().unwrap() = CopyInSession { stmt: Some(copy), buffer: Vec::new() };
            responses.push(Response::CopyIn(CopyResponse::new(0, cols, vec![0; cols])));
            continue;
        }
        if let Some(copy) = parse_copy_stdout(sql) {
            responses.extend(handle_copy_stdout(client, &conn, &copy).await?);
            continue;
        }
        if let Some(response) = try_handle_wire_notify_command(&self.notify_registry, client, sql)? {
            responses.push(response);
            continue;
        }

        let mut stmt = conn.prepare(sql)
            .map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;

        if stmt.num_columns() == 0 || is_pg_non_query(sql) {
            responses.push(execute_non_query(&mut stmt, sql)?);
        } else {
            let header = Arc::new(build_field_info(&stmt, &Format::UnifiedText));
            responses.push(execute_query(stmt, header)?);
        }
        self.cleanup_dropped_schema_file(sql, None);
    }
    Ok(responses)
}

// after
async fn do_query<C>(&self, client: &mut C, query: &str) -> PgWireResult<Vec<Response>>
where ... {
    flush_pending_wire_notifications(&self.notify_registry, client).await?;
    let conn = self.conn.clone();

    let statements = turso_parser_pg::split_statements(query)
        .map_err(|e| PgWireError::UserError(Box::new(parse_error_info(&e.to_string()))))?;

    let mut responses = Vec::new();
    for sql in &statements {
        // Any per-statement error becomes a Response::Error pushed onto the
        // batch, and stops processing subsequent statements — matching real
        // PostgreSQL simple-query-protocol semantics — instead of using `?`
        // to discard responses already collected for earlier, successful
        // statements in this same batch (whose side effects already happened).
        macro_rules! try_stmt {
            ($expr:expr) => {
                match $expr {
                    Ok(v) => v,
                    Err(PgWireError::UserError(info)) => {
                        responses.push(Response::Error(info));
                        break;
                    }
                    Err(e) => return Err(e), // framework/IO errors still propagate
                }
            };
        }

        if let Some(copy) = parse_copy_stdin(sql) {
            let cols = try_stmt!(copy_column_count(&conn, &copy));
            *self.copy_in.lock().unwrap() = CopyInSession { stmt: Some(copy), buffer: Vec::new() };
            responses.push(Response::CopyIn(CopyResponse::new(0, cols, vec![0; cols])));
            continue;
        }
        if let Some(copy) = parse_copy_stdout(sql) {
            let copy_responses = try_stmt!(handle_copy_stdout(client, &conn, &copy).await);
            responses.extend(copy_responses);
            continue;
        }
        if let Some(response) = try_stmt!(try_handle_wire_notify_command(&self.notify_registry, client, sql)) {
            responses.push(response);
            continue;
        }

        let mut stmt = try_stmt!(conn.prepare(sql).map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e)))));

        let result = if stmt.num_columns() == 0 || is_pg_non_query(sql) {
            execute_non_query(&mut stmt, sql)
        } else {
            let header = Arc::new(build_field_info(&stmt, &Format::UnifiedText));
            execute_query(stmt, header)
        };
        responses.push(try_stmt!(result));
        self.cleanup_dropped_schema_file(sql, None);
    }
    Ok(responses)
}
```

The `macro_rules!` avoids repeating the match-and-break five times; if the team prefers no local macros, write it as a plain closure returning `ControlFlow` or duplicate the match arm five times instead — check `parser_pg/src/translator.rs` for the house style on repeated-error-handling before deciding. `break` exits the `for` loop but not the function, so `Ok(responses)` at the end still returns everything collected so far, including the pushed `Response::Error`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro wire_simple_query_batch_reports_completed_statements_before_error -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): multi-statement batches report completed statements' responses before a later error"
```

**Notes:** Verify pgwire's `_on_query` (`src/api/query.rs:274-279`) correctly sets error transaction status when it sees `Response::Error` mid-vector and doesn't also expect it to be the *last* element — it iterates the full `Vec` in order, so no issue, but confirm no statements after the pushed `Response::Error` sneak into `responses` (they can't, since we `break` immediately).

---

### Task A14 (M1): Yield periodically during long row-streaming/COPY loops so they don't starve the tokio runtime

**Files:**
- Modify: `cli/pg_server.rs:906-959` (`execute_query`'s `stream::try_unfold`), `:695-821` (`handle_copy_stdout`)

**Interfaces:**
- Standalone.

Hard to test deterministically/non-flakily in CI (it's a scheduler-fairness property, not a correctness property). Write a best-effort latency test but treat it as advisory, not a hard gate.

- [ ] **Step 1: Write the advisory test**

```rust
const WIRE_PORT_YIELD_FAIRNESS: u16 = 31432;

#[test]
fn wire_large_query_does_not_starve_concurrent_small_query() {
    let port = wire_port(WIRE_PORT_YIELD_FAIRNESS);
    let mut server = start_pgmicro_server(port);

    let mut big_client = PgTestClient::connect(port);
    big_client.query_raw(
        "CREATE TABLE big AS WITH RECURSIVE s(n) AS \
         (SELECT 1 UNION ALL SELECT n+1 FROM s WHERE n < 200000) SELECT n FROM s",
    );

    let mut small_client = PgTestClient::connect(port);

    big_client.send_query("SELECT count(*) FROM big"); // large scan, no result read yet
    let start = std::time::Instant::now();
    let resp = small_client.query_raw("SELECT 1"); // trivial, should not queue behind the scan
    let elapsed = start.elapsed();

    assert!(!response_has_error(&resp));
    // Advisory bound, not a hard correctness assertion — tune/relax if flaky.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "small query took {elapsed:?} while a large query was in flight — possible scheduler starvation"
    );

    big_client.read_until_ready();
    server.kill().ok();
    server.wait().ok();
}
```

- [ ] **Step 2: Run test to observe current (possibly flaky) baseline**

Run: `cargo test -p pgmicro wire_large_query_does_not_starve_concurrent_small_query -- --nocapture`
Expected: may pass or fail depending on scheduler behavior today — this is advisory, not a strict TDD gate.

- [ ] **Step 3: Implement**

```rust
// cli/pg_server.rs:906-959, inside the try_unfold closure's Ok(StepResult::Row) arm — before
Ok(StepResult::Row) => {
    let row = state.stmt.row().expect("row must be present after StepResult::Row");
    let data_row = encode_row(row, &state.header).map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;
    return Ok(Some((data_row, Some(state))));
}

// after — yield periodically so long scans don't monopolize a worker thread
Ok(StepResult::Row) => {
    let row = state.stmt.row().expect("row must be present after StepResult::Row");
    let data_row = encode_row(row, &state.header).map_err(|e| PgWireError::UserError(Box::new(limbo_error_to_pg(e))))?;
    tokio::task::yield_now().await;
    return Ok(Some((data_row, Some(state))));
}
```

Same pattern in `handle_copy_stdout`'s row loop (`cli/pg_server.rs:734-765`): insert `tokio::task::yield_now().await` inside the `Ok(StepResult::Row) => { ... rows.push(values); }` arm — batch every ~256 rows if per-row proves too chatty (measure before deciding).

- [ ] **Step 4: Re-run the advisory test and full suite**

Run: `cargo test -p pgmicro`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): yield periodically during long row-streaming loops to avoid starving the tokio runtime"
```

**Notes:** `on_copy_done`'s single call into `conn.handle_pg_copy_data(...)` (`cli/pg_server.rs:864-876`) can't be yielded *inside* without a `core/` change (it's one opaque synchronous call) — if this proves to matter in practice for large COPY payloads, the real fix is `tokio::task::spawn_blocking` around that call, a separate larger decision (moves it off the tokio worker pool entirely, changing panic/cancellation semantics). Flag as a follow-up, don't bundle into this task.

---

### Task A15 (M2): TLS key loading only accepts PKCS8 keys

**Files:**
- Modify: `cli/pg_server.rs:392-418` (`load_tls_acceptor`)

**Interfaces:**
- Standalone.

Not practical as a `pgmicro/tests/pgmicro.rs` wire test (would need to generate a PKCS1 keypair and a real TLS handshake in-test) — use a lower-level unit test directly on `load_tls_acceptor` with a fixture PKCS1 PEM file checked into `cli/` test fixtures.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn load_tls_acceptor_accepts_pkcs1_rsa_key() {
    let cert = Path::new("testdata/tls/pkcs1_cert.pem");
    let key = Path::new("testdata/tls/pkcs1_key.pem"); // "BEGIN RSA PRIVATE KEY"
    assert!(load_tls_acceptor(cert, key).is_ok());
}
```

Generate the fixture files first: `openssl req -x509 -newkey rsa:2048 -traditional -keyout cli/testdata/tls/pkcs1_key.pem -out cli/testdata/tls/pkcs1_cert.pem -days 3650 -nodes -subj "/CN=pgmicro-test"`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro load_tls_acceptor_accepts_pkcs1_rsa_key -- --nocapture`
Expected: FAIL — `pkcs8_private_keys` rejects a `"BEGIN RSA PRIVATE KEY"` (PKCS1) PEM block.

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:392-418)
fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<pgwire::tokio::TlsAcceptor> {
    use rustls_pemfile::{certs, pkcs8_private_keys};
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    ...
    let key = pkcs8_private_keys(&mut BufReader::new(File::open(key_path)?))
        .map(|key| key.map(PrivateKeyDer::from))
        .collect::<Result<Vec<PrivateKeyDer>, IoError>>()?
        .into_iter()
        .next()
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "no private key found"))?;
    ...
}

// after
fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<pgwire::tokio::TlsAcceptor> {
    use rustls_pemfile::{certs, private_key};
    use rustls_pki_types::CertificateDer;
    ...
    let key = private_key(&mut BufReader::new(File::open(key_path)?))?
        .ok_or_else(|| IoError::new(ErrorKind::InvalidInput, "no private key found"))?;
    ...
}
```

`rustls_pemfile::private_key` (present in `rustls-pemfile 2.2.0`, already the pinned version per `Cargo.lock:5490-5492`) auto-detects PKCS1/PKCS8/SEC1 in one call — no new dependency needed.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro load_tls_acceptor_accepts_pkcs1_rsa_key -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs cli/testdata/tls/
git commit -S -m "fix(wire): TLS key loading accepts PKCS1 RSA keys, not just PKCS8"
```

---

### Task A16 (M3): NOTIFY subscriber leak if `process_error`'s socket write fails — folds into Task A5's cleanup tail

**Files:**
- Modify: `cli/pg_server.rs:1626-1646`

**Interfaces:**
- Standalone (independent of C4 conceptually, but folds into the same tail region Task A5 touches — land together to avoid a merge conflict, and so there's exactly one place doing "connection teardown," not two divergent copies).

Hard to trigger deterministically (requires an actual socket-write failure mid-error-handling) — recommend a code-review-verified change rather than an automated regression test, per the "hard to test" allowance. The one practical automated angle is a unit test on `TursoPgNotifyRegistry::unregister_wire_session` idempotency (safe to call twice / on an unknown pid) — not a reproduction of the leak itself.

- [ ] **Step 1: Write the idempotency test**

```rust
#[test]
fn unregister_wire_session_is_idempotent_for_unknown_pid() {
    let hub = Arc::new(turso_core::PgNotifyHub::default());
    let registry = TursoPgNotifyRegistry::new(hub);
    registry.unregister_wire_session(999); // never registered — must not panic
}
```

- [ ] **Step 2: Run test to confirm current baseline behavior**

Run: `cargo test -p pgmicro unregister_wire_session_is_idempotent_for_unknown_pid -- --nocapture`
Expected: PASS today already (idempotency isn't the bug) — this test guards the invariant the fix below relies on (calling `unregister_wire_session` from an early-return path must be safe even if it's also called again in the normal tail).

- [ ] **Step 3: Implement**

```rust
// before (cli/pg_server.rs:1626-1646)
if let Some(Ok(msg)) = msg {
    let is_extended_query = match socket.state() { ... };
    if let Err(mut e) = process_message(msg, socket, ..., cancel_handler.clone()).await {
        error_handler.on_error(socket, &mut e);
        process_error(socket, e, is_extended_query).await?;
    }
} else {
    break;
}
}

notify_registry.unregister_wire_session(socket.pid_and_secret_key().0);
Ok(())

// after
if let Some(Ok(msg)) = msg {
    let is_extended_query = match socket.state() { ... };
    if let Err(mut e) = process_message(msg, socket, ..., cancel_handler.clone()).await {
        error_handler.on_error(socket, &mut e);
        if let Err(io_err) = process_error(socket, e, is_extended_query).await {
            let (pid, _secret) = socket.pid_and_secret_key();
            notify_registry.unregister_wire_session(pid);
            return Err(io_err);
        }
    }
} else {
    break;
}
}

notify_registry.unregister_wire_session(socket.pid_and_secret_key().0);
Ok(())
```

- [ ] **Step 4: Run full suite**

Run: `cargo test -p pgmicro`
Expected: PASS.

- [ ] **Step 5: Commit** (bundle with, or immediately after, Task A5)

```bash
git add cli/pg_server.rs
git commit -S -m "fix(wire): unregister NOTIFY subscriber on early-return path when process_error's socket write fails"
```

**Notes:** Once Task A5 lands (which also adds `cancel_registry_ref.unregister(...)` and `conn_for_cleanup.close()` at this same tail), fold this early-return cleanup into that same helper/scope so there's exactly one teardown path, not two divergent copies.

---

### Workstream A sequencing notes

1. Tasks A1-A6 (C4) are foundational and must land as one coherent sequence — A3 depends on A2 depends on A1; A4/A5/A6 depend on A3. Do not parallelize within this chain; a single engineer should own A1-A6 start to finish.
2. Task A7 (C5) and Task A8 (C6) both depend on A3/A4 landing first (A7's isolation test is meaningless without per-socket connections; A8 is explicitly a regression test for A3's side effect). Once A1-A6 land, A7/A8/A9 can run in parallel — different code regions (`TursoCancelRegistry` vs. `copy_in` field vs. `execute_non_query`).
3. Task A16 (M3) textually conflicts with Task A5's edit to the same `process_socket_with_notify` tail — land A16 immediately before or after A5, in the same PR/review pass, not independently.
4. Tasks A10-A12 (H22 binary encoding) are independent of the C4/C5/C6 chain and can run in parallel with it on a second engineer, once Task A2's `encode_row`→`encode_value` format-threading groundwork (introduced inline in Task A10) is settled — A11/A12 depend on A10's `format: FieldFormat` parameter existing.
5. Task A13 (H23) and Task A14 (M1) and Task A15 (M2) are fully standalone — assign to whichever engineer has capacity, any order, no shared files with the C4-C6 chain (A13 touches `do_query`, A14 touches `execute_query`'s stream and `handle_copy_stdout`, A15 touches `load_tls_acceptor` — none overlap A1-A9's edited regions except A5/A9 both touch `execute_non_query`'s neighborhood loosely; verify no literal line-range collision before parallelizing A9 against A5).

---

## Workstream C — Dialect/Dispatch

**Worktree:** `wt-dialect` · **Primary file:** `core/pg_dispatch.rs` · **Test command:** `cargo test -p core_tester --test integration_tests integration::postgres`

All line numbers below verified against the current tree (read-only). Test harness confirmed via `tests/integration/postgres/dialect.rs` (`TempDatabase` / `db.connect_limbo()` / `#[turso_macros::test(mvcc)]`, file-backed via `db.path: PathBuf`) and `tests/integration/postgres/catalog.rs` (existing `PREPARE`/`EXECUTE` coverage, `#[turso_macros::test]`).

**Landing order:** C8 (highest priority, foundational) → H18 (independent, can run in parallel with C8) → H19 (independent, can run in parallel with C8/H18) → Task C-cleanup-1 (consumes C8, must land after) → Task C-cleanup-2 (consumes C8, must land after, test-only).

### Task C1 (C8): DROP SCHEMA empty-check + centralized file lifecycle

**Files:**
- Modify: `core/pg_dispatch.rs:225-246` (`handle_pg_drop_schema`)
- Modify: `core/pg_dispatch.rs:212-223` (reuse `schema_file_path`, unchanged, add sibling helper)
- Test: `tests/integration/postgres/dialect.rs` (new tests, near existing `test_postgres_drop_schema*` block at lines 447-547)

**Interfaces:**
- Consumes: from Workstream B, Task B15 (H20) — `PgDropSchemaStmt { names: Vec<String>, if_exists: bool, cascade: bool }` (i.e. `.name: String` becomes `.names: Vec<String>`). This task does **not** touch `parser_pg/src/translator.rs`; it only changes how `core/pg_dispatch.rs` consumes the struct. **Sequencing dependency (cross-workstream interface #1, see top of plan):** this task must land after or together with Workstream B's Task B15. If sequencing requires landing this first, temporarily consume `std::slice::from_ref(&stmt.name)` as a shim, then delete the shim when Task B15 lands.
- Produces: `unlink_schema_file` — the file-lifecycle contract that Task C2 (delete `cli/pg_server.rs`'s duplicate parser) depends on. Do not start Task C2 until this task merges.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/integration/postgres/dialect.rs

#[turso_macros::test(mvcc)]
fn test_postgres_drop_schema_non_cascade_non_empty_error(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE SCHEMA dropme").unwrap();
    conn.execute("CREATE TABLE dropme.t1 (id INTEGER PRIMARY KEY)")
        .unwrap();

    // DROP SCHEMA without CASCADE on a non-empty schema must error — matches
    // the existing "public" schema behavior in
    // test_postgres_drop_schema_public_no_cascade_error. Before the fix this
    // silently succeeds and detaches without dropping/erroring.
    let result = conn.execute("DROP SCHEMA dropme");
    assert!(
        result.is_err(),
        "DROP SCHEMA without CASCADE on a non-empty schema should error, got {result:?}"
    );

    // The schema must still be usable (proves it was NOT silently detached).
    conn.execute("INSERT INTO dropme.t1 (id) VALUES (1)").unwrap();
}

#[turso_macros::test(mvcc)]
fn test_postgres_drop_schema_deletes_backing_file_and_data(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE SCHEMA s").unwrap();
    conn.execute("CREATE TABLE s.t (x INTEGER)").unwrap();
    conn.execute("INSERT INTO s.t VALUES (42)").unwrap();

    let schema_file = db
        .path
        .parent()
        .unwrap()
        .join("turso-postgres-schema-s.db");
    assert!(
        schema_file.exists(),
        "schema file should exist after CREATE SCHEMA"
    );

    conn.execute("DROP SCHEMA s CASCADE").unwrap();
    assert!(
        !schema_file.exists(),
        "schema file must be deleted after DROP SCHEMA, found leftover {schema_file:?}"
    );

    // C8: recreating the schema must NOT resurrect the old table/data. Before
    // the fix, the file was left on disk and silently reattached here.
    conn.execute("CREATE SCHEMA s").unwrap();
    let result = conn.query("SELECT * FROM s.t");
    assert!(
        result.is_err(),
        "table from the dropped schema must not reappear in the recreated schema, got {result:?}"
    );
}

// Depends on Workstream B's Task B15 (Vec<String> support) landing first.
// If that lands after this task, add this test in the same PR that wires the
// two together, not before (it will not compile otherwise).
#[turso_macros::test(mvcc)]
fn test_postgres_drop_schema_multi_name(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE SCHEMA a").unwrap();
    conn.execute("CREATE SCHEMA b").unwrap();

    conn.execute("DROP SCHEMA a, b").unwrap();

    // Both schemas must actually be gone — re-creating each must succeed,
    // proving both were dropped, not just the first (the original bug: only
    // "a" was dropped, "b" silently survived with no error).
    conn.execute("CREATE SCHEMA a").unwrap();
    conn.execute("CREATE SCHEMA b").unwrap();
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_drop_schema_non_cascade_non_empty_error integration::postgres::test_postgres_drop_schema_deletes_backing_file_and_data -- --nocapture`
Expected: FAIL — non-cascade non-empty DROP SCHEMA silently succeeds today, and the backing file is left on disk after DROP SCHEMA, causing data resurrection on recreate.

- [ ] **Step 3: Implement**

```rust
// core/pg_dispatch.rs — before (lines 225-246)

    /// Handle DROP SCHEMA in PostgreSQL mode.
    /// For "public": drops all user tables from main DB.
    /// For other schemas: drops all tables, then DETACHes.
    fn handle_pg_drop_schema(self: &Arc<Self>, stmt: &PgDropSchemaStmt) -> Result<()> {
        let name = stmt.name.to_lowercase();
        validate_schema_name(&name)?;
        if name == "public" {
            return self.handle_pg_drop_schema_public(stmt.cascade);
        }
        if !self.is_attached(&name) {
            if stmt.if_exists {
                return Ok(());
            }
            return Err(LimboError::ParseError(format!(
                "schema \"{name}\" does not exist"
            )));
        }
        if stmt.cascade {
            self.drop_all_tables_in_schema(&name)?;
        }
        self.detach_database(&name)
    }
```

```rust
// core/pg_dispatch.rs — after

    /// Handle DROP SCHEMA in PostgreSQL mode. Supports comma-separated
    /// schema lists (`DROP SCHEMA a, b`); each name is dropped independently
    /// and in order (not atomic across names — see Notes).
    /// For "public": drops all user tables from main DB.
    /// For other schemas: enforces the same non-cascade-non-empty check as
    /// "public" (C8), drops all tables when CASCADE, DETACHes, then deletes
    /// the backing `turso-postgres-schema-<name>.db(-wal/-shm)` files so a
    /// later CREATE SCHEMA of the same name starts empty instead of
    /// resurrecting the old data.
    fn handle_pg_drop_schema(self: &Arc<Self>, stmt: &PgDropSchemaStmt) -> Result<()> {
        for name in &stmt.names {
            self.drop_one_schema(name, stmt.if_exists, stmt.cascade)?;
        }
        Ok(())
    }

    fn drop_one_schema(self: &Arc<Self>, name: &str, if_exists: bool, cascade: bool) -> Result<()> {
        let name = name.to_lowercase();
        validate_schema_name(&name)?;
        if name == "public" {
            return self.handle_pg_drop_schema_public(cascade);
        }
        if !self.is_attached(&name) {
            if if_exists {
                return Ok(());
            }
            return Err(LimboError::ParseError(format!(
                "schema \"{name}\" does not exist"
            )));
        }
        // Match the "public" schema's non-cascade-non-empty check: dropping
        // a populated schema without CASCADE must error, not silently
        // detach and leave the data reachable through a stale file.
        let table_names = self.list_user_tables(Some(&name))?;
        if !cascade && !table_names.is_empty() {
            return Err(LimboError::ParseError(format!(
                "cannot drop schema \"{name}\" because other objects depend on it"
            )));
        }
        if cascade {
            self.drop_all_tables_in_schema(&name)?;
        }
        self.detach_database(&name)?;
        // Only unlink after a successful detach, so a failed DROP SCHEMA
        // never touches the file (mirrors the previous wire-only behavior
        // tested by pgmicro/tests/pgmicro.rs::wire_drop_schema_keeps_file_on_failure).
        self.unlink_schema_file(&name);
        Ok(())
    }

    /// Delete the backing `.db`/`-wal`/`-shm` files for a detached schema.
    /// No-op for in-memory main databases, which have no schema files.
    /// Must be called only *after* `detach_database` has succeeded.
    fn unlink_schema_file(&self, schema_name: &str) {
        if self.db.path == ":memory:" {
            return;
        }
        let path = std::path::PathBuf::from(self.schema_file_path(schema_name));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_drop_schema_non_cascade_non_empty_error integration::postgres::test_postgres_drop_schema_deletes_backing_file_and_data integration::postgres::test_postgres_drop_schema_multi_name -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run the wire-level regression tests to confirm externally-observable behavior is preserved**

Run: `cargo test -p pgmicro wire_drop_schema_keeps_file_on_failure wire_prepared_drop_schema_deletes_file -- --nocapture`
Expected: PASS unmodified — these assert file exists/not-exists behavior preserved by this design; run explicitly, don't skip.

- [ ] **Step 6: Commit**

```bash
git add core/pg_dispatch.rs tests/integration/postgres/dialect.rs
git commit -S -m "fix(dialect): DROP SCHEMA enforces non-cascade-non-empty check and deletes backing files, preventing data resurrection"
```

**Notes:**
- **Non-atomicity across multiple names**: `DROP SCHEMA a, b` where `b` doesn't exist (no `IF EXISTS`) will leave `a` dropped and error on `b` — PostgreSQL treats the whole statement as one implicit transaction and would roll back `a` too. This codebase already accepts this pattern elsewhere (`handle_pg_truncate`, `core/pg_dispatch.rs:292-311`, loops without wrapping in `BEGIN`/`COMMIT`), so this is consistent with existing conventions, not a new gap — call it out explicitly in the PR description so it's a documented, not accidental, limitation.
- `unlink_schema_file` intentionally swallows `remove_file` errors (`let _ =`) — matches the existing tolerance in `cli/pg_server.rs::delete_schema_file` (logs a warning instead). `core/connection.rs` already uses `tracing::debug!` extensively, so adding a `tracing::warn!` here is a reasonable minor addition, not a blocker — not required for this task.

---

### Task C2 (H18): SET/SHOW allowlist for common PostgreSQL client GUCs

**Files:**
- Modify: `core/pg_dispatch.rs:70-89` (`try_prepare_pg`'s SET/SHOW blocks)
- Test: `tests/integration/postgres/dialect.rs` (near `test_postgres_pragma`, lines 5-41)

**Interfaces:** None — self-contained within `core/pg_dispatch.rs`. Independent of Task C1; can run in parallel.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/integration/postgres/dialect.rs

#[turso_macros::test(mvcc)]
fn test_postgres_set_common_client_gucs_are_noop(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    // Real PG client libraries/ORMs/poolers issue these on connect. They
    // must not error even though pgmicro has no PRAGMA equivalent — before
    // the fix, each of these errors "Not a valid pragma name".
    for set_sql in [
        "SET client_encoding = 'UTF8'",
        "SET application_name = 'psql'",
        "SET DateStyle = 'ISO, MDY'",
        "SET TimeZone = 'UTC'",
        "SET extra_float_digits = 3",
        "SET standard_conforming_strings = on",
        "SET statement_timeout = 30000",
    ] {
        conn.execute(set_sql)
            .unwrap_or_else(|e| panic!("{set_sql} should be a no-op, got error: {e:?}"));
    }

    // SHOW must also not error for the same GUCs.
    conn.query("SHOW client_encoding")
        .expect("SHOW client_encoding should not error");

    // The connection must still be fully usable afterward.
    let mut rows = conn.query("SELECT 1").unwrap().unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
}

#[turso_macros::test(mvcc)]
fn test_postgres_set_unknown_pragma_still_errors(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    // A name that is neither a known no-op GUC nor a real Turso pragma must
    // still error — the allowlist must not become a silent catch-all.
    let result = conn.execute("SET totally_made_up_setting_xyz = 1");
    assert!(result.is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_set_common_client_gucs_are_noop -- --nocapture`
Expected: FAIL — `SET client_encoding = 'UTF8'` errors "Not a valid pragma name" today.

- [ ] **Step 3: Implement**

```rust
// core/pg_dispatch.rs — before (lines 70-89)

        if let Some(set_stmt) = try_extract_set(&parse_result) {
            if set_stmt.name == "search_path" {
                self.handle_pg_set_search_path(&set_stmt)?;
                return Ok(Some(self.prepare_sqlite_sql("SELECT 0 WHERE 0")?));
            }
            let value = set_stmt
                .values
                .first()
                .ok_or_else(|| LimboError::ParseError("SET statement missing value".to_string()))?;
            let pragma_sql = format!("PRAGMA {} = {}", set_stmt.name, value);
            return Ok(Some(self.prepare_sqlite_sql(&pragma_sql)?));
        }

        if let Some(show_stmt) = try_extract_show(&parse_result) {
            if show_stmt.name == "search_path" {
                return Ok(Some(self.prepare_pg_show_search_path()?));
            }
            let pragma_sql = format!("PRAGMA {}", show_stmt.name);
            return Ok(Some(self.prepare_sqlite_sql(&pragma_sql)?));
        }
```

```rust
// core/pg_dispatch.rs — after

        if let Some(set_stmt) = try_extract_set(&parse_result) {
            if set_stmt.name == "search_path" {
                self.handle_pg_set_search_path(&set_stmt)?;
                return Ok(Some(self.prepare_sqlite_sql("SELECT 0 WHERE 0")?));
            }
            // Common PostgreSQL session GUCs sent by real client libraries
            // on connect (encoding negotiation, display formatting,
            // timeouts). pgmicro has no PRAGMA equivalent for these; accept
            // as a no-op instead of erroring through the PRAGMA passthrough.
            if is_pg_noop_guc(&set_stmt.name) {
                return Ok(Some(self.prepare_sqlite_sql("SELECT 0 WHERE 0")?));
            }
            let value = set_stmt
                .values
                .first()
                .ok_or_else(|| LimboError::ParseError("SET statement missing value".to_string()))?;
            let pragma_sql = format!("PRAGMA {} = {}", set_stmt.name, value);
            return Ok(Some(self.prepare_sqlite_sql(&pragma_sql)?));
        }

        if let Some(show_stmt) = try_extract_show(&parse_result) {
            if show_stmt.name == "search_path" {
                return Ok(Some(self.prepare_pg_show_search_path()?));
            }
            if is_pg_noop_guc(&show_stmt.name) {
                return Ok(Some(self.prepare_sqlite_sql("SELECT ''")?));
            }
            let pragma_sql = format!("PRAGMA {}", show_stmt.name);
            return Ok(Some(self.prepare_sqlite_sql(&pragma_sql)?));
        }
```

Add near the top of the `impl Connection` block (or as a free function above it):

```rust
/// PostgreSQL session GUCs that pgmicro accepts but does not act on. These
/// are routinely set by real PG client libraries/ORMs/connection poolers on
/// connect and have no SQLite/Turso PRAGMA equivalent worth wiring up yet.
/// Accepting them as no-ops (instead of erroring through the PRAGMA
/// passthrough) lets those clients connect at all. Names not in this list
/// still fall through to the PRAGMA passthrough and are validated normally.
fn is_pg_noop_guc(name: &str) -> bool {
    const PG_NOOP_GUCS: &[&str] = &[
        "client_encoding",
        "application_name",
        "datestyle",
        "timezone",
        "extra_float_digits",
        "standard_conforming_strings",
        "statement_timeout",
        "idle_in_transaction_session_timeout",
        "lock_timeout",
        "client_min_messages",
        "bytea_output",
        "intervalstyle",
        "row_security",
    ];
    PG_NOOP_GUCS.contains(&name.to_ascii_lowercase().as_str())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_set_common_client_gucs_are_noop integration::postgres::test_postgres_set_unknown_pragma_still_errors -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/pg_dispatch.rs tests/integration/postgres/dialect.rs
git commit -S -m "fix(dialect): accept common PostgreSQL client GUCs as no-ops so real client libraries can connect"
```

**Notes:**
- Deliberately conservative: unlisted GUCs (including real Turso pragmas like `journal_mode`) keep falling through to the existing PRAGMA passthrough unchanged, so no existing test/behavior regresses — `test_postgres_set_unknown_pragma_still_errors` pins that down.
- Follow-up worth flagging separately (not in this task): the SET→PRAGMA passthrough for *non*-allowlisted names still lets a PG client toggle real SQLite-engine pragmas (`journal_mode`, `locking_mode`, `cache_size`, etc.) by disguising them as `SET`. Whether that's desired (power-user escape hatch) or should be tightened to a strict PG-only GUC allowlist is a product decision — raise it to whoever owns the wire-protocol security model rather than deciding it here.

---

### Task C3 (H19): EXECUTE `$N` placeholder substitution corrupts queries

**Finding reproduced live:** `PREPARE p AS SELECT $10; EXECUTE p(5);` returns `50`.

**Files:**
- Modify: `core/pg_dispatch.rs:421-437` (`handle_pg_execute`)
- Test: `tests/integration/postgres/catalog.rs` (next to `test_pg_prepare_execute_deallocate`, lines 1287-1306 — this is where existing PREPARE/EXECUTE coverage already lives, not `dialect.rs`)

**Interfaces:** None for this minimal fix (self-contained in `core/pg_dispatch.rs`). Independent of Task C1/C2; can run in parallel. See Notes for a recommended follow-up architectural fix with a cross-workstream dependency on `parser_pg/src/translator.rs`.

- [ ] **Step 1: Write the failing test**

```rust
// tests/integration/postgres/catalog.rs

#[turso_macros::test]
fn test_pg_prepare_execute_placeholder_number_collision(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = 'postgres'").unwrap();

    // $1 must not be textually matched inside $10. This is the exact H19
    // corruption: the old `str::replacen("$1", ...)` spliced the bound
    // value into the middle of the "$10" token instead of leaving it alone,
    // silently returning 50 instead of erroring.
    conn.execute("PREPARE p AS SELECT $10").unwrap();
    let result = conn.prepare("EXECUTE p(5)");
    assert!(
        result.is_err(),
        "EXECUTE p(5) supplies only one argument but the prepared body \
         references $10; this must error, not silently return 50, got {result:?}"
    );

    // A prepared statement with 10 real parameters, where a higher-numbered
    // placeholder appears before a lower-numbered one in the SQL text, must
    // bind each placeholder to its own distinct value.
    conn.execute("PREPARE p10 AS SELECT $10, $1, $2, $3, $4, $5, $6, $7, $8, $9")
        .unwrap();
    let mut stmt = conn
        .prepare("EXECUTE p10(1, 2, 3, 4, 5, 6, 7, 8, 9, 999)")
        .unwrap();
    let StepResult::Row = stmt.step().unwrap() else {
        panic!("expected execute row");
    };
    let row = stmt.row().unwrap();
    let Value::Numeric(Numeric::Integer(tenth)) = row.get_value(0) else {
        panic!("expected integer result for $10");
    };
    assert_eq!(
        *tenth, 999,
        "$10 must bind to the 10th EXECUTE argument, not get mangled by $1 substitution"
    );
    let Value::Numeric(Numeric::Integer(first)) = row.get_value(1) else {
        panic!("expected integer result for $1");
    };
    assert_eq!(*first, 1);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_pg_prepare_execute_placeholder_number_collision -- --nocapture`
Expected: FAIL — `EXECUTE p(5)` on `PREPARE p AS SELECT $10` silently returns `50` instead of erroring.

- [ ] **Step 3: Implement**

```rust
// core/pg_dispatch.rs — before (lines 421-437)

    fn handle_pg_execute(self: &Arc<Self>, stmt: &PgExecuteStmt) -> Result<Statement> {
        let sql = self.pg_prepared.read().get(&stmt.name)?;
        if stmt.params.is_empty() {
            return self.prepare(sql.as_str());
        }
        let mut prepared_sql = sql;
        for (i, param) in stmt.params.iter().enumerate() {
            let placeholder = format!("${}", i + 1);
            if !prepared_sql.contains(&placeholder) {
                return Err(LimboError::ParseError(format!(
                    "prepared statement has no placeholder {placeholder}"
                )));
            }
            prepared_sql = prepared_sql.replacen(&placeholder, param, 1);
        }
        self.prepare(prepared_sql.as_str())
    }
```

```rust
// core/pg_dispatch.rs — after

    fn handle_pg_execute(self: &Arc<Self>, stmt: &PgExecuteStmt) -> Result<Statement> {
        let sql = self.pg_prepared.read().get(&stmt.name)?;
        if stmt.params.is_empty() {
            return self.prepare(sql.as_str());
        }
        let prepared_sql = substitute_execute_params(&sql, &stmt.params)?;
        self.prepare(prepared_sql.as_str())
    }
```

Add as a free function in `core/pg_dispatch.rs`:

```rust
/// Substitute `$1`..`$N` placeholder tokens in a stored PREPARE body with
/// the literal SQL text of each EXECUTE argument. Scans for whole
/// `$<digits>` tokens rather than doing textual `str::replace`, so `$1` can
/// never match inside `$10`/`$11`/etc. (H19: the previous
/// `replacen("$1", ...)` implementation silently corrupted queries with
/// >=10 parameters, or with a higher-numbered placeholder appearing before
/// a lower-numbered one in the SQL text).
fn substitute_execute_params(sql: &str, params: &[String]) -> Result<String> {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            // Safe: [start, end) is ASCII digits only, so this always parses.
            let n: usize = sql[start..end].parse().unwrap();
            let param = params.get(n.wrapping_sub(1)).ok_or_else(|| {
                LimboError::ParseError(format!("prepared statement has no placeholder ${n}"))
            })?;
            out.push_str(param);
            i = end;
        } else {
            // Advance one char at a time to stay char-boundary safe for
            // non-ASCII SQL text (identifiers, string literals, comments).
            let ch = sql[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_pg_prepare_execute_placeholder_number_collision -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/pg_dispatch.rs tests/integration/postgres/catalog.rs
git commit -S -m "fix(dialect): EXECUTE placeholder substitution no longer corrupts \$1/\$10 collisions"
```

**Notes:**
- **This is the minimal, in-scope fix** — it eliminates the demonstrated substring-collision corruption with a single-pass token scan, no new dependencies (no regex crate needed).
- **Residual limitation, not fixed here**: the scanner still doesn't distinguish `$1` inside a string literal/comment from a real placeholder (e.g. `PREPARE p AS SELECT '$1 is a variable'` would still get corrupted) — this is a pre-existing limitation of doing placeholder substitution on raw SQL text at all, and it's the reason to prefer the architectural fix below.
- **Architectural fix investigated, recommended as follow-up (separate task, cross-workstream, NOT scheduled in this plan)**: `parser_pg/src/translator.rs:2354-2357` already translates `ParamRef` (`$1`, `$2`, ...) into `ast::Expr::Variable(ast::Variable::indexed(...))` — a genuine Turso bind parameter, identical to how the wire protocol's extended-query path (`bind_portal_parameters` in `cli/pg_server.rs`) already binds real typed values via `Statement::bind_at` (`core/statement.rs:873`). The correct long-term fix is: stop stringifying EXECUTE arguments into re-quoted SQL text at all — `self.prepare(sql)` the stored PREPARE body as-is (with its real `$N` placeholders), then `stmt.bind_at(NonZero::new(i+1), value)` for each argument, exactly like `handle_pg_copy_data` (`core/pg_dispatch.rs:387-411`) already does for COPY. This is immune to every substring/ordering issue by construction. It requires `PgExecuteStmt.params` to carry typed `Value`s (or at least enough type info to construct one) instead of the current deparsed-to-text `Vec<String>` from `deparse_default_expr` (`parser_pg/src/translator.rs:5750`) — a translator-side interface change with the same shape as Task C1's `Vec<String>` dependency. Flag this to whoever owns Workstream B as a follow-up; do not fold it into this task, since it touches `parser_pg/src/translator.rs` (out of this task's file boundary) and changes the `PgExecuteStmt` public shape that Task C1 also depends on the stability of.

---

### Task C4: Delete `cli/pg_server.rs`'s duplicate DROP SCHEMA re-parser

**Finding:** MEDIUM — fragile duplicate parser for file cleanup (contingent on Task C1/C8)

**Files:**
- Modify: `cli/pg_server.rs:262-340` (delete `cleanup_dropped_schema_file`, `drop_schema_name`, `resolve_drop_schema_token`, `delete_schema_file`; remove the now-dead `db_file` field from `TursoPgHandler`)
- Modify: `cli/pg_server.rs:106-112` (remove `db_file: self.db_file.clone(),` from the `TursoPgHandler` construction)
- Modify: `cli/pg_server.rs:473`, `cli/pg_server.rs:535` (remove the two call sites)
- Modify: `cli/pg_server.rs:1826-1857` (delete `test_drop_schema_name_literal`, `test_drop_schema_name_parameterized`, `test_drop_schema_name_missing_parameter`)

**Interfaces:** Consumes Task C1's `core::Connection::handle_pg_drop_schema` file-lifecycle change. Do not start this task until Task C1 has merged — deleting this code first would remove file cleanup entirely with nothing to replace it.

**Note on line numbers:** this task touches `cli/pg_server.rs` line ranges that overlap Workstream A's line numbers as captured at plan-writing time (Workstream A also edits `TursoPgHandler`'s field list, in Task A3). Since Workstream A's C4 (connection-per-socket) work restructures `TursoPgHandler`/`TursoPgServer` substantially, land Workstream A's Tasks A1-A6 **before** this task, and re-verify all line numbers/field lists against the post-Workstream-A state of `cli/pg_server.rs` rather than trusting the numbers below verbatim.

- [ ] **Step 1: Confirm no new test needed — deletion is validated by pre-existing tests**

Run (before making any change, to record the baseline): `cargo test -p pgmicro wire_drop_schema_keeps_file_on_failure wire_prepared_drop_schema_deletes_file -- --nocapture`
Expected: PASS (baseline, pre-deletion).

- [ ] **Step 2: Implement the deletion**

```rust
// cli/pg_server.rs — delete lines 262-340 in full:
//   struct CopyInSession { ... }             <- keep, unrelated (256-260)
//   struct TursoPgHandler { ... db_file: String, ... }   <- keep struct, drop the db_file field only
//   impl TursoPgHandler {
//       fn cleanup_dropped_schema_file(...) { ... }      <- DELETE (272-286)
//   }
//   fn drop_schema_name(...) { ... }                     <- DELETE (291-303)
//   fn resolve_drop_schema_token(...) { ... }             <- DELETE (305-322)
//   fn delete_schema_file(...) { ... }                    <- DELETE (324-340)

// before (struct field, line 264):
struct TursoPgHandler {
    conn: Arc<Mutex<Arc<Connection>>>,
    db_file: String,
    query_parser: Arc<NoopQueryParser>,
    copy_in: Arc<Mutex<CopyInSession>>,
    notify_registry: Arc<TursoPgNotifyRegistry>,
}

// after:
struct TursoPgHandler {
    conn: Arc<Mutex<Arc<Connection>>>,
    query_parser: Arc<NoopQueryParser>,
    copy_in: Arc<Mutex<CopyInSession>>,
    notify_registry: Arc<TursoPgNotifyRegistry>,
}
```

(If Workstream A's Task A3 has already landed, `conn` is `Arc<Connection>` not `Arc<Mutex<Arc<Connection>>>` — match whatever the field's actual current type is; the `db_file` field removal is the substantive change here, independent of that type.)

```rust
// before (construction site, lines 106-112):
            handler: Arc::new(TursoPgHandler {
                conn: self.conn.clone(),
                db_file: self.db_file.clone(),
                query_parser: Arc::new(NoopQueryParser::new()),
                copy_in: Arc::new(Mutex::new(CopyInSession::default())),
                notify_registry: notify_registry.clone(),
            }),

// after:
            handler: Arc::new(TursoPgHandler {
                conn: self.conn.clone(),
                query_parser: Arc::new(NoopQueryParser::new()),
                copy_in: Arc::new(Mutex::new(CopyInSession::default())),
                notify_registry: notify_registry.clone(),
            }),
```

```rust
// before (call site, line 473, inside SimpleQueryHandler::do_query):
            // Only delete the backing schema file once the statement has
            // executed successfully. Deleting before execution risks orphaning
            // schema metadata if execution fails.
            self.cleanup_dropped_schema_file(sql, None);
        }

        Ok(responses)

// after:
        }

        Ok(responses)
```

```rust
// before (call site, lines 532-537, inside ExtendedQueryHandler::do_query):
        if stmt.num_columns() == 0 || is_pg_non_query(query) {
            let response = execute_non_query(&mut stmt, query)?;
            // Delete the backing schema file only after successful execution.
            self.cleanup_dropped_schema_file(query, Some(portal));
            return Ok(response);
        }

// after:
        if stmt.num_columns() == 0 || is_pg_non_query(query) {
            let response = execute_non_query(&mut stmt, query)?;
            return Ok(response);
        }
```

- [ ] **Step 3: Run the pre-existing regression tests**

Run: `cargo test -p pgmicro wire_drop_schema_keeps_file_on_failure wire_prepared_drop_schema_deletes_file -- --nocapture`
Expected: PASS — unchanged from Step 1's baseline, confirming file-cleanup behavior now comes from `core/` (Task C1) instead of `cli/`.

- [ ] **Step 4: Run clippy to catch dangling references**

Run: `cargo clippy --workspace --all-features --all-targets -- --deny=warnings`
Expected: clean — catches any unused `db_file` field or unused imports left dangling. Re-grep `cli/pg_server.rs` for any other construction site referencing `db_file` before merging (only one construction site was found at plan-writing time, but re-verify at execution time since Workstream A's changes may have shifted things).

- [ ] **Step 5: Commit**

```bash
git add cli/pg_server.rs
git commit -S -m "refactor(wire): delete duplicate DROP SCHEMA re-parser now that core owns file lifecycle"
```

**Notes:** This task is pure risk-reduction (deleting a duplicate/fragile parser) — it changes no externally observable behavior, hence no new test.

---

### Task C5: Schema-file leak outside the wire server (REPL/NAPI/embedders)

**Finding:** MEDIUM — same root cause as C8, resolved by consequence once Task C1 lands.

**Files:**
- Test only: `pgmicro/tests/pgmicro.rs` (new test + small local helper, near `wire_drop_schema_keeps_file_on_failure` at line 1347)

**Interfaces:** Consumes Task C1. Write and run this test only after Task C1 merges (it should already pass once Task C1 lands; if it doesn't, that's a signal Task C1's fix is incomplete for the non-wire entry point, not that a second fix is needed here). No production code changes beyond Task C1 — `core/pg_dispatch.rs::handle_pg_drop_schema` is the single dispatch point for **every** frontend (REPL via `pgmicro/src/main.rs`, wire server via `cli/pg_server.rs`, NAPI, and any other embedder), so Task C1's fix in `core/` closes this gap for all of them simultaneously. `pgmicro/src/main.rs` currently has zero schema-file cleanup code (confirmed: only reference to `turso-postgres-schema-` is the `\dn` display-name strip at line 156) — that absence is exactly why this was leaking, and Task C1 fixes it without any REPL-side change needed.

- [ ] **Step 1: Write the test (should fail on pre-Task-C1 code, pass after Task C1)**

```rust
// pgmicro/tests/pgmicro.rs

#[test]
fn repl_drop_schema_deletes_backing_file() {
    let dir = std::env::temp_dir().join(format!("pgmicro-repl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    let db_path = dir.join("main.db");
    let schema_file = dir.join("turso-postgres-schema-replschema.db");

    let output = run_pgmicro_with_db(
        &db_path,
        b"CREATE SCHEMA replschema;\nCREATE TABLE replschema.t(x INT);\nDROP SCHEMA replschema CASCADE;\n",
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(
        !schema_file.exists(),
        "REPL DROP SCHEMA must delete the backing file, same as the wire server \
         does (see wire_drop_schema_keeps_file_on_failure); found leftover {}",
        schema_file.display()
    );

    std::fs::remove_dir_all(&dir).ok();
}

fn run_pgmicro_with_db(db_path: &std::path::Path, input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pgmicro"))
        .arg(db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run pgmicro");

    let mut stdin = child.stdin.take().expect("failed to take stdin");
    stdin.write_all(input).expect("failed to write stdin");
    drop(stdin);

    child.wait_with_output().expect("failed to wait for output")
}
```

`pgmicro/src/main.rs`'s existing `run_pgmicro` test helper hardcodes `:memory:` (line 6-7 of `pgmicro/tests/pgmicro.rs`), so this new `run_pgmicro_with_db` helper is needed for a file-backed REPL test — added locally rather than changing the shared `run_pgmicro` signature (would require touching every existing call site for no benefit).

- [ ] **Step 2: Run test — must pass once Task C1 has landed**

Run: `cargo test -p pgmicro repl_drop_schema_deletes_backing_file -- --nocapture`
Expected: PASS (assuming Task C1 already merged). If this fails after Task C1 lands, that means Task C1's `handle_pg_drop_schema` fix isn't actually reached from the REPL path (e.g., some REPL-specific dialect/connection wiring bypasses `core::Connection::prepare`) — treat that as a bug in Task C1's landing, not a new finding.

- [ ] **Step 3: Commit**

```bash
git add pgmicro/tests/pgmicro.rs
git commit -S -m "test(repl): confirm DROP SCHEMA deletes backing file from the REPL entry point, not just the wire server"
```

---

### Workstream C sequencing notes

1. Task C1 (C8) is highest priority (data resurrection) and foundational — it must land before Task C4 and Task C5, both of which consume its file-lifecycle contract.
2. Task C1 has a **cross-workstream dependency** on Workstream B's Task B15 (H20, `PgDropSchemaStmt.names: Vec<String>`) — see cross-workstream interface #1 at the top of this plan. Land together, or use the documented `std::slice::from_ref` shim if Task C1 must land first.
3. Task C2 (H18) and Task C3 (H19) are fully independent of Task C1 and of each other — different code regions within `core/pg_dispatch.rs` (SET/SHOW block vs. `handle_pg_execute`). Assign to separate engineers, run in parallel with Task C1.
4. Task C4 depends on Task C1 merging first, and additionally should land after Workstream A's Tasks A1-A6 (connection-per-socket refactor) since both touch `TursoPgHandler`'s field list in `cli/pg_server.rs` — re-verify line numbers against the post-Workstream-A state before executing Task C4.
5. Task C5 is test-only and depends on Task C1; can run any time after Task C1 merges, in parallel with Task C4.

---

## Workstream D — Core Semantics

**Worktree:** `wt-semantics` · **Primary files:** `core/vdbe/value.rs`, `core/vdbe/execute.rs` · **Test command:** `cargo test -p core_tester --test integration_tests integration::postgres`

Both tasks are independent silent-correctness fixes (wrong results returned without error) that share the same dialect-branching pattern (`program.connection.get_sql_dialect() == crate::SqlDialect::Postgres`). Each touches a distinct code region — no file-level conflicts between them.

**Note on H6:** the review's `~*`/`!~*` case-insensitive-regex finding (H6) is **not** a task in this workstream — it is fully owned by Workstream B's Task B11, which adopts an inline `(?i)`-flag translator fix (zero `core/` changes) per the cross-workstream decision documented at the top of this plan. Do not re-implement it here; a competing `core/regexp.rs`-level `regexp_i()` approach was considered and explicitly rejected.

### Task D1 (H14): PG `LIKE` must be case-sensitive (SQLite `LIKE` stays case-insensitive)

**Files:**
- Modify: `core/vdbe/value.rs:1178-1218` (`exec_like`), `core/vdbe/value.rs:1394-1399` (`LIKE_INFO` const)
- Modify: `core/vdbe/execute.rs:7076-7143` (`ScalarFunc::Like` arm inside `op_function`)
- Test: `tests/integration/postgres/dialect.rs`

**Interfaces:**
- Produces: `Value::exec_like_with_case(pattern, text, escape, case_sensitive) -> Result<bool, LimboError>` — new function; `Value::exec_like` becomes a thin `case_sensitive: false` wrapper around it, so all ~15+ existing call sites (tests, benches) are unaffected.
- Consumes: `Connection::get_sql_dialect()` (existing, `core/connection.rs:2892-2894`), `crate::SqlDialect::Postgres` (existing enum).

- [ ] **Step 1: Write the failing tests**

```rust
// tests/integration/postgres/dialect.rs

#[turso_macros::test(mvcc)]
fn test_postgres_like_is_case_sensitive(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'Alice')").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'ALICE')").unwrap();
    conn.execute("INSERT INTO t VALUES (3, 'alice')").unwrap();

    // PostgreSQL LIKE is case-sensitive: only the exact-case row matches.
    let mut rows = conn
        .query("SELECT name FROM t WHERE name LIKE '%Alice%' ORDER BY id")
        .unwrap()
        .unwrap();

    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    let Value::Text(val) = row.get_value(0) else {
        panic!("expected text");
    };
    assert_eq!(val.as_str(), "Alice");

    // No more rows — 'ALICE' and 'alice' must NOT match a case-sensitive LIKE.
    let StepResult::Done = rows.step().unwrap() else {
        panic!("LIKE matched a differently-cased row — PG LIKE must be case-sensitive");
    };
}

#[turso_macros::test(mvcc)]
fn test_sqlite_like_stays_case_insensitive(db: TempDatabase) {
    // Regression guard: the dialect branch must not change SQLite's own
    // default LIKE behavior (case-insensitive ASCII match).
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'Alice')").unwrap();

    let mut rows = conn
        .query("SELECT name FROM t WHERE name LIKE '%ALICE%'")
        .unwrap()
        .unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("SQLite-dialect LIKE regressed to case-sensitive");
    };
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_like_is_case_sensitive -- --nocapture`
Expected: FAIL — `LIKE '%Alice%'` matches all three rows today under Postgres dialect.

- [ ] **Step 3: Implement**

```rust
// core/vdbe/value.rs — before (lines 1178-1218)
pub fn exec_like(pattern: &str, text: &str, escape: Option<char>) -> Result<bool, LimboError> {
    const MAX_LIKE_PATTERN_LENGTH: usize = 50000;
    if pattern.len() > MAX_LIKE_PATTERN_LENGTH {
        return Err(LimboError::Constraint(
            "LIKE or GLOB pattern too complex".to_string(),
        ));
    }
    let pattern = sqlite_text_prefix(pattern);
    let text = sqlite_text_prefix(text);

    let has_escape = escape.is_some_and(|e| pattern.contains(e));

    // 1. Exact match (no wildcards)
    if !has_escape && !pattern.contains(['%', '_']) {
        return Ok(pattern.eq_ignore_ascii_case(text));
    }

    // 2. Fast Path: 'abc%' (Prefix)
    if !has_escape
        && pattern.ends_with('%')
        && !pattern[..pattern.len() - 1].contains(['%', '_'])
    {
        let prefix = &pattern[..pattern.len() - 1];
        if text.len() >= prefix.len() && text.is_char_boundary(prefix.len()) {
            return Ok(text[..prefix.len()].eq_ignore_ascii_case(prefix));
        }
    }

    // 3. Fast Path: '%abc' (Suffix)
    if !has_escape && pattern.starts_with('%') && !pattern[1..].contains(['%', '_']) {
        let suffix = &pattern[1..];
        let start = text.len().wrapping_sub(suffix.len());
        if text.len() >= suffix.len() && text.is_char_boundary(start) {
            return Ok(text[start..].eq_ignore_ascii_case(suffix));
        }
    }

    Ok(pattern_compare(pattern, text, &LIKE_INFO, escape) == CompareResult::Match)
}
```

```rust
// core/vdbe/value.rs — after
pub fn exec_like(pattern: &str, text: &str, escape: Option<char>) -> Result<bool, LimboError> {
    Self::exec_like_with_case(pattern, text, escape, false)
}

/// `case_sensitive`: PostgreSQL's `LIKE`/`~~` is case-sensitive (only `ILIKE`
/// is case-insensitive). SQLite's `LIKE` defaults to ASCII case-insensitive.
/// Callers must pass the flag derived from the active `SqlDialect` — `false`
/// preserves existing SQLite-dialect behavior exactly.
pub fn exec_like_with_case(
    pattern: &str,
    text: &str,
    escape: Option<char>,
    case_sensitive: bool,
) -> Result<bool, LimboError> {
    const MAX_LIKE_PATTERN_LENGTH: usize = 50000;
    if pattern.len() > MAX_LIKE_PATTERN_LENGTH {
        return Err(LimboError::Constraint(
            "LIKE or GLOB pattern too complex".to_string(),
        ));
    }
    let pattern = sqlite_text_prefix(pattern);
    let text = sqlite_text_prefix(text);

    let has_escape = escape.is_some_and(|e| pattern.contains(e));
    let eq = |a: &str, b: &str| {
        if case_sensitive {
            a == b
        } else {
            a.eq_ignore_ascii_case(b)
        }
    };

    // 1. Exact match (no wildcards)
    if !has_escape && !pattern.contains(['%', '_']) {
        return Ok(eq(pattern, text));
    }

    // 2. Fast Path: 'abc%' (Prefix)
    if !has_escape
        && pattern.ends_with('%')
        && !pattern[..pattern.len() - 1].contains(['%', '_'])
    {
        let prefix = &pattern[..pattern.len() - 1];
        if text.len() >= prefix.len() && text.is_char_boundary(prefix.len()) {
            return Ok(eq(&text[..prefix.len()], prefix));
        }
    }

    // 3. Fast Path: '%abc' (Suffix)
    if !has_escape && pattern.starts_with('%') && !pattern[1..].contains(['%', '_']) {
        let suffix = &pattern[1..];
        let start = text.len().wrapping_sub(suffix.len());
        if text.len() >= suffix.len() && text.is_char_boundary(start) {
            return Ok(eq(&text[start..], suffix));
        }
    }

    let info = if case_sensitive { &LIKE_INFO_CASE_SENSITIVE } else { &LIKE_INFO };
    Ok(pattern_compare(pattern, text, info, escape) == CompareResult::Match)
}
```

```rust
// core/vdbe/value.rs — before (lines 1394-1399)
const LIKE_INFO: PatternInfo = PatternInfo {
    match_all: '%',
    match_one: '_',
    match_set: None,
    no_case: true,
};
```

```rust
// core/vdbe/value.rs — after
const LIKE_INFO: PatternInfo = PatternInfo {
    match_all: '%',
    match_one: '_',
    match_set: None,
    no_case: true,
};

const LIKE_INFO_CASE_SENSITIVE: PatternInfo = PatternInfo {
    match_all: '%',
    match_one: '_',
    match_set: None,
    no_case: false,
};
```

```rust
// core/vdbe/execute.rs — before (line 7140)
let matches = Value::exec_like(&pattern_cow, &match_cow, escape_char)?;
```

```rust
// core/vdbe/execute.rs — after
let case_sensitive = program.connection.get_sql_dialect() == crate::SqlDialect::Postgres;
let matches = Value::exec_like_with_case(&pattern_cow, &match_cow, escape_char, case_sensitive)?;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_like_is_case_sensitive integration::postgres::test_sqlite_like_stays_case_insensitive -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Build check for missed callers**

Run: `cargo build -p turso_core`
Expected: clean — confirms no other internal caller of `exec_like` was missed by the wrapper approach.

- [ ] **Step 6: Commit**

```bash
git add core/vdbe/value.rs core/vdbe/execute.rs tests/integration/postgres/dialect.rs
git commit -S -m "fix(semantics): PostgreSQL LIKE is case-sensitive, matching PG semantics"
```

**Notes:**
- `program: &Program` is already a named parameter of `op_function`, and `program.connection` is already dereferenced a few lines below for `ScalarFunc::LastInsertRowid` (`execute.rs:7075-7076`) — no signature change needed.
- Deliberately did not change `exec_like`'s existing signature — kept it a thin wrapper so none of the existing call sites in `core/vdbe/value.rs` tests, `core/benches/sql_functions/value.rs`, and `core/benches/sql_functions/likeop.rs` need to change.
- Reuses the existing `PatternInfo.no_case` mechanism already used to distinguish LIKE (`no_case: true`) from GLOB (`no_case: false`), rather than inventing new machinery.
- Only one production call site found (`execute.rs:7140`); re-grep `LikeOperator::Like` in `core/translate/` before merging in case a range-scan optimizer transform was added since this review — the optimizer only prunes candidates and always re-validates with the real LIKE function, so no optimizer changes are expected, but verify.

---

### Task D2 (H17): Plain integer/float division by zero must error in Postgres dialect

**Files:**
- Modify: `core/vdbe/execute.rs:410-424` (`op_divide`)
- Modify: `cli/pg_server.rs:1782-1795` (`classify_constraint_sqlstate`)
- Test: `tests/integration/postgres/dialect.rs`

**Interfaces:**
- Consumes: `Connection::get_sql_dialect()`, `crate::SqlDialect::Postgres` (same accessor as Task D1), `Numeric::from_value` (existing).
- Produces: none consumed elsewhere in this plan.

- [ ] **Step 1: Write the failing tests**

```rust
// tests/integration/postgres/dialect.rs

#[turso_macros::test(mvcc)]
fn test_postgres_division_by_zero_errors(db: TempDatabase) {
    let conn = db.connect_limbo();
    conn.execute("PRAGMA sql_dialect = postgres").unwrap();

    // Plain integer division by zero must raise a PG-style error, not
    // silently return NULL (SQLite's default division-by-zero behavior).
    let err = limbo_exec_rows_fallible(&db, &conn, "SELECT 1/0").unwrap_err();
    assert!(
        matches!(err, LimboError::Constraint(ref msg) if msg.contains("division by zero")),
        "expected division-by-zero Constraint error, got: {err:?}"
    );

    // Float division by zero must also error.
    let err = limbo_exec_rows_fallible(&db, &conn, "SELECT 1.0/0").unwrap_err();
    assert!(
        matches!(err, LimboError::Constraint(ref msg) if msg.contains("division by zero")),
        "expected division-by-zero Constraint error for float division, got: {err:?}"
    );

    // A column-typed division by zero must error too, not just literals.
    conn.execute("CREATE TABLE t (a INT4, b INT4)").unwrap();
    conn.execute("INSERT INTO t VALUES (10, 0)").unwrap();
    let err = limbo_exec_rows_fallible(&db, &conn, "SELECT a/b FROM t").unwrap_err();
    assert!(
        matches!(err, LimboError::Constraint(ref msg) if msg.contains("division by zero")),
        "expected division-by-zero Constraint error for column division, got: {err:?}"
    );
}

#[turso_macros::test(mvcc)]
fn test_sqlite_division_by_zero_still_returns_null(db: TempDatabase) {
    // Regression guard: SQLite dialect must keep returning NULL, not error.
    let conn = db.connect_limbo();
    let mut rows = conn.query("SELECT 1/0").unwrap().unwrap();
    let StepResult::Row = rows.step().unwrap() else {
        panic!("expected row");
    };
    let row = rows.row().unwrap();
    assert_eq!(row.get_value(0), &Value::Null);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_division_by_zero_errors -- --nocapture`
Expected: FAIL — `SELECT 1/0` returns NULL today under Postgres dialect instead of erroring.

- [ ] **Step 3: Implement**

```rust
// core/vdbe/execute.rs — before (lines 410-424)
pub fn op_divide(
    _program: &Program,
    state: &mut ProgramState,
    insn: &Insn,
    _pager: &Arc<Pager>,
) -> Result<InsnFunctionStepResult> {
    load_insn!(Divide { lhs, rhs, dest }, insn);
    state.registers[*dest].set_value(
        state.registers[*lhs]
            .get_value()
            .exec_divide(state.registers[*rhs].get_value()),
    );
    state.pc += 1;
    Ok(InsnFunctionStepResult::Step)
}
```

```rust
// core/vdbe/execute.rs — after
pub fn op_divide(
    program: &Program,
    state: &mut ProgramState,
    insn: &Insn,
    _pager: &Arc<Pager>,
) -> Result<InsnFunctionStepResult> {
    load_insn!(Divide { lhs, rhs, dest }, insn);
    let rhs_value = state.registers[*rhs].get_value();

    if program.connection.get_sql_dialect() == crate::SqlDialect::Postgres {
        let is_zero = match Numeric::from_value(rhs_value) {
            Some(Numeric::Integer(0)) => true,
            Some(Numeric::Float(f)) => f64::from(f) == 0.0,
            _ => false,
        };
        if is_zero {
            return Err(LimboError::Constraint("division by zero".to_string()));
        }
    }

    state.registers[*dest]
        .set_value(state.registers[*lhs].get_value().exec_divide(rhs_value));
    state.pc += 1;
    Ok(InsnFunctionStepResult::Step)
}
```

```rust
// cli/pg_server.rs — before (lines 1782-1795)
fn classify_constraint_sqlstate(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("unique") || lower.contains("primary key") {
        "23505"
    } else if lower.contains("not null") {
        "23502"
    } else if lower.contains("check") {
        "23514"
    } else if lower.contains("foreign key") {
        "23503"
    } else {
        "23000"
    }
}
```

```rust
// cli/pg_server.rs — after
fn classify_constraint_sqlstate(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("division by zero") {
        "22012"
    } else if lower.contains("unique") || lower.contains("primary key") {
        "23505"
    } else if lower.contains("not null") {
        "23502"
    } else if lower.contains("check") {
        "23514"
    } else if lower.contains("foreign key") {
        "23503"
    } else {
        "23000"
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p core_tester --test integration_tests integration::postgres::test_postgres_division_by_zero_errors integration::postgres::test_sqlite_division_by_zero_still_returns_null -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add core/vdbe/execute.rs cli/pg_server.rs tests/integration/postgres/dialect.rs
git commit -S -m "fix(semantics): plain integer/float division by zero errors under Postgres dialect"
```

**Notes:**
- `program.connection.get_sql_dialect()` reuses the same accessor pattern as Task D1 — add `use crate::SqlDialect;` or fully qualify as `crate::SqlDialect::Postgres` (recommend fully qualifying to avoid touching the import list, matching Task D1's approach).
- **Bonus fix bundled in**: today even the already-correct `NUMERIC`/`INTERVAL`/`MONEY` division-by-zero errors map to SQLSTATE `23000` via `classify_constraint_sqlstate`'s fallback branch, not PG's actual `22012` (`division_by_zero`). This task's `classify_constraint_sqlstate` change fixes the SQLSTATE for *all* division-by-zero paths, not just the new one — verify against the existing `NumericDiv`/`IntervalDiv`/`MoneyDiv` error paths (`core/vdbe/execute.rs:8062-8068`, `core/interval/mod.rs:92`, `core/money/mod.rs:53`) too, since they share this same mapping function.
- **Known gap, explicitly out of scope**: `core/incremental/expr_compiler.rs:129` also calls `Value::exec_divide` directly (incremental/materialized view maintenance) with no obvious access to `Connection`/dialect in that context — it will keep silently returning NULL on division by zero even under Postgres dialect for incrementally-maintained views. Flag this in the PR description as a known follow-up; do not expand this task's scope to fix it without checking whether `expr_compiler.rs` has any dialect context to thread through.
- `exec_divide` (`core/numeric/mod.rs:152-165`) is intentionally left untouched — the dialect branch belongs at the execution call site, matching Task D1's pattern.

---

### Workstream D sequencing notes

1. Tasks D1 and D2 are fully independent — different files/regions (`value.rs`+`execute.rs`'s LIKE path vs. `execute.rs`'s divide op + `pg_server.rs`'s SQLSTATE map). Assign to two engineers, any order, no coordination needed. (H6, the third semantics finding originally considered for this workstream, is implemented in Workstream B's Task B11 instead — see the note at the top of this workstream.)
2. D2's `cli/pg_server.rs:classify_constraint_sqlstate` edit is textually close to nothing else touched by Workstream A or C in this plan — confirmed no overlapping line ranges with Workstream A's Task A9/A10 edits (different functions) or Workstream C's tasks (different file region entirely, `pg_dispatch.rs` vs `pg_server.rs`).
3. **Findings explicitly NOT specced in this workstream** (feature gaps, not silent-correctness regressions — each needs its own two-plan-rule follow-up per CLAUDE.md, not a task here): `CREATE SEQUENCE`/`nextval`/`currval`/`setval` support, full `TIMESTAMPTZ` timezone semantics (`AT TIME ZONE`, session `TimeZone` GUC, offset-aware storage), advisory locks (`pg_advisory_lock` family), and `INT4` 32-bit range enforcement. Each requires either a new Turso-core primitive (sequence object type, timezone-aware temporal type, session-scoped lock registry) or a deliberate scope decision (extending the `smallint`-style CHECK-based custom-type pattern to `int4`, and its performance/compatibility tradeoff for a widely-used base type). Scope each as a Turso-core plan (no PostgreSQL framing) paired with a separate pgmicro plan, per CLAUDE.md's "two-plan rule" — do not fold into this bug-fix plan.

---

## Workstream F — REPL/Packaging

**Worktree:** `wt-repl` · **Primary files:** `pgmicro/src/main.rs`, `npm/pgmicro/cli.js`, `npm/pgmicro/index.js` · **Test command:** `cargo test -p pgmicro`

Two sub-chains exist within this workstream: (1) a strict `run_stdin()`/`consume()` sequencing chain (F1 → F4 → F5), since F1 and F4 both edit adjacent lines in the same loop body; (2) everything else is independent. See "Workstream F sequencing notes" at the end.

### Task F1 (H33): Report invalid-UTF-8 stdin as an error instead of silent EOF

**Files:**
- Modify: `pgmicro/src/main.rs:985-997` (`Repl::run_stdin`)
- Test: `pgmicro/tests/pgmicro.rs`

**Interfaces:**
- Consumes: `Repl::had_error` (existing field), `Repl::consume` (existing method — Task F4 changes its signature; this task does not).
- Produces: none new — this task lands first in the `run_stdin` chain, so Task F4's edits are written against the *post-F1* state of `run_stdin`.

- [ ] **Step 1: Write the failing test**

```rust
// pgmicro/tests/pgmicro.rs

// ---------------------------------------------------------------------------
// Invalid input: non-UTF-8 bytes on stdin
// ---------------------------------------------------------------------------

#[test]
fn invalid_utf8_stdin_reports_error_and_exits_nonzero() {
    // Reproduction: valid statement, then a raw invalid-UTF-8 byte pair, then
    // another valid statement that must NOT be silently dropped/lost.
    let output = run_pgmicro(b"SELECT 1;\n\xff\xfe\nSELECT 2;\n");
    assert_ne!(
        output.status.code(),
        Some(0),
        "invalid UTF-8 on stdin must not exit 0"
    );
    let out = stdout(&output);
    assert!(out.contains('1'), "the valid prefix should still execute, got: {out}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid") || stderr.contains("UTF-8"),
        "expected a clear error about invalid input, got stderr: {stderr}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro invalid_utf8_stdin_reports_error_and_exits_nonzero -- --nocapture`
Expected: FAIL — invalid UTF-8 on stdin today causes `read_line` to silently return `Ok(0)`/error-swallowed EOF, exiting 0 with no error message.

- [ ] **Step 3: Implement**

```rust
// pgmicro/src/main.rs — before (lines 985-997)
fn run_stdin(&mut self) {
    let stdin = std::io::stdin();
    loop {
        let prev_len = self.input_buf.len();
        if std::io::BufRead::read_line(&mut stdin.lock(), &mut self.input_buf).unwrap_or(0) == 0
        {
            self.consume(true);
            break;
        }
        self.read_state.process(&self.input_buf[prev_len..]);
        self.consume(false);
    }
}
```

```rust
// pgmicro/src/main.rs — after
fn run_stdin(&mut self) {
    let stdin = std::io::stdin();
    loop {
        let prev_len = self.input_buf.len();
        match std::io::BufRead::read_line(&mut stdin.lock(), &mut self.input_buf) {
            Ok(0) => {
                self.consume(true);
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error: invalid input on stdin: {e}");
                self.had_error = true;
                break;
            }
        }
        self.read_state.process(&self.input_buf[prev_len..]);
        self.consume(false);
    }
}
```

No other changes needed: `had_error` is an existing `Repl` field, and `main()` (line ~1072-1074) already does `if !interactive && repl.had_error { std::process::exit(1); }`, so setting it here wires up the correct exit code for free.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro invalid_utf8_stdin_reports_error_and_exits_nonzero -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add pgmicro/src/main.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(repl): report invalid UTF-8 stdin as an error instead of silently exiting 0"
```

**Notes:** This task's edit region overlaps Task F4's edit region in the same `run_stdin()` loop body (F4 changes the trailing `self.consume(false)` call into an `if`/`break`, not the `read_line` match this task edits) — land this task first; Task F4 rebases on top to avoid a merge conflict on adjacent lines.

---

### Task F2 (H34a): Strip/fold quoted identifiers in `\d` / `\d+` arguments

**Files:**
- Modify: `pgmicro/src/main.rs:611-676` (`handle_meta_command`), new helper added just above it
- Test: `pgmicro/tests/pgmicro.rs`

**Interfaces:** None — self-contained to `pgmicro/src/main.rs`. Fully independent of every other Workstream F task; no ordering constraint.

- [ ] **Step 1: Write the failing tests**

```rust
// pgmicro/tests/pgmicro.rs

// ---------------------------------------------------------------------------
// Meta-commands: \d with quoted / mixed-case identifiers
// ---------------------------------------------------------------------------

#[test]
fn d_quoted_identifier_strips_surrounding_quotes() {
    let output = run_pgmicro(b"CREATE TABLE \"Foo\"(bar TEXT);\n\\d \"Foo\"\n");
    assert_eq!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(
        !out.contains("not found"),
        "\\d \"Foo\" should find the quoted table, got: {out}"
    );
    assert!(out.contains("bar"), "expected column 'bar' in: {out}");
}

#[test]
fn d_quoted_identifier_unescapes_doubled_quotes() {
    let output = run_pgmicro(b"CREATE TABLE \"a\"\"b\"(x INT);\n\\d \"a\"\"b\"\n");
    assert_eq!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(
        !out.contains("not found"),
        "\\d \"a\"\"b\" should find the table, got: {out}"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pgmicro d_quoted_identifier_strips_surrounding_quotes d_quoted_identifier_unescapes_doubled_quotes -- --nocapture`
Expected: FAIL — `\d "Foo"` looks up the table literally including the quote characters today, reporting "not found".

- [ ] **Step 3: Implement**

```rust
// pgmicro/src/main.rs — before (lines 636-649, inside handle_meta_command)
        "\\d+" => {
            if arg.is_empty() {
                cmd_list_tables_extended(conn, w);
            } else {
                cmd_describe_table_extended(conn, arg, w);
            }
        }
        "\\d" => {
            if arg.is_empty() {
                cmd_list_tables(conn, w);
            } else {
                cmd_describe_table(conn, arg, w);
            }
        }
```

```rust
// pgmicro/src/main.rs — after
        "\\d+" => {
            if arg.is_empty() {
                cmd_list_tables_extended(conn, w);
            } else {
                cmd_describe_table_extended(conn, &unquote_identifier(arg), w);
            }
        }
        "\\d" => {
            if arg.is_empty() {
                cmd_list_tables(conn, w);
            } else {
                cmd_describe_table(conn, &unquote_identifier(arg), w);
            }
        }
```

Add the helper directly above `handle_meta_command` (before line 611):

```rust
/// Normalize a psql-style meta-command identifier argument: a double-quoted
/// argument has its quotes stripped and internal `""` unescaped to `"`,
/// preserving case, matching PostgreSQL quoted-identifier rules. An
/// unquoted argument is folded to lowercase, matching PostgreSQL's
/// unquoted-identifier folding (mirrors what libpg_query already does to
/// `relname` when the table is created).
fn unquote_identifier(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].replace("\"\"", "\"")
    } else {
        trimmed.to_lowercase()
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pgmicro d_quoted_identifier_strips_surrounding_quotes d_quoted_identifier_unescapes_doubled_quotes -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add pgmicro/src/main.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(repl): \\d/\\d+ strip and unescape quoted identifiers instead of matching literally"
```

**Notes:** No `core/` changes. Does **not** fix Task F3's underlying issue — the catalog match is still case-insensitive today, so `\d FOO` finding `Foo` keeps "working" for the wrong reason until Task F3 (or a real core fix) lands; that's fine, this task only closes the quote-stripping gap.

---

### Task F3 (H34b): Loud, specific error on case-insensitive identifier collision (flag-and-document only)

**Files:**
- Modify: `core/translate/schema.rs:1158-1174`
- Test: `pgmicro/tests/pgmicro.rs`

**Root cause (do not attempt to fix here):** `core/util.rs:121-125`'s `normalize_ident()` unconditionally lowercases every identifier for both the SQLite and Postgres dialects — it has no concept of PG's quoted-vs-unquoted distinction. Table storage identity is keyed off this normalized name, so `"Foo"` and `foo` collide by construction. Fixing that is a real engine change (per CLAUDE.md's "two-plan rule": a Turso-core plan with no PG mention, then a pgmicro plan on top) — **out of scope for this task**. This task only makes the resulting error message honest about *why* the collision happened.

**Interfaces:** None — self-contained to `core/translate/schema.rs`. Independent of every other Workstream F task. **This is a `core/` change** — flag for extra review/CI attention per CLAUDE.md's "minimize core/ changes."

- [ ] **Step 1: Write the failing test**

```rust
// pgmicro/tests/pgmicro.rs

// ---------------------------------------------------------------------------
// Case-insensitive identifier collision: clear error, not generic "already exists"
// ---------------------------------------------------------------------------

#[test]
fn quoted_vs_unquoted_name_collision_reports_case_insensitivity() {
    let output = run_pgmicro(b"CREATE TABLE \"Foo\"(bar TEXT);\nCREATE TABLE foo(baz INT);\n");
    assert_ne!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(
        out.contains("case-insensitive") || out.contains("case insensitive"),
        "expected the error to explain the case-insensitive collision, got: {out}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pgmicro quoted_vs_unquoted_name_collision_reports_case_insensitivity -- --nocapture`
Expected: FAIL — today the error is a bare `table foo already exists`, with no mention of case-insensitivity.

- [ ] **Step 3: Implement**

```rust
// core/translate/schema.rs — before (lines 1158-1174)
    // Check for name conflicts with existing schema objects
    if let Some(object_type) =
        resolver.with_schema(database_id, |s| s.get_object_type(&normalized_tbl_name))
    {
        match object_type {
            // IF NOT EXISTS suppresses errors for table/view conflicts
            SchemaObjectType::Table | SchemaObjectType::View if if_not_exists => {
                return Ok(());
            }
            _ => {
                let type_str = match object_type {
                    SchemaObjectType::Table => "table",
                    SchemaObjectType::View => "view",
                    SchemaObjectType::Index => "index",
                };
                bail_parse_error!("{} {} already exists", type_str, normalized_tbl_name);
            }
        }
    }
```

```rust
// core/translate/schema.rs — after
    // Check for name conflicts with existing schema objects
    if let Some(object_type) =
        resolver.with_schema(database_id, |s| s.get_object_type(&normalized_tbl_name))
    {
        match object_type {
            // IF NOT EXISTS suppresses errors for table/view conflicts
            SchemaObjectType::Table | SchemaObjectType::View if if_not_exists => {
                return Ok(());
            }
            _ => {
                let type_str = match object_type {
                    SchemaObjectType::Table => "table",
                    SchemaObjectType::View => "view",
                    SchemaObjectType::Index => "index",
                };
                let raw_name = tbl_name.name.as_str();
                if raw_name != normalized_tbl_name {
                    // The requested name differs only in case/quoting from an
                    // existing object. Identifiers are folded case-insensitively
                    // here (unlike real PostgreSQL), so say so explicitly instead
                    // of a bare "already exists" that looks like a true duplicate.
                    bail_parse_error!(
                        "{} {} already exists (identifiers are case-insensitive here: \"{}\" collides with existing \"{}\")",
                        type_str, normalized_tbl_name, raw_name, normalized_tbl_name
                    );
                }
                bail_parse_error!("{} {} already exists", type_str, normalized_tbl_name);
            }
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pgmicro quoted_vs_unquoted_name_collision_reports_case_insensitivity -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run full core test suite (required for any `core/` change per CLAUDE.md)**

Run: `cargo test -p core_tester`
Expected: PASS — confirms this error-message change doesn't alter any existing test's exact-match assertions elsewhere.

- [ ] **Step 6: Commit**

```bash
git add core/translate/schema.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(core): explain case-insensitive identifier collisions in CREATE TABLE errors"
```

**Notes:** The identical pattern likely also applies to `CREATE TYPE` (`core/translate/schema.rs:2517-2526`) and `CREATE DOMAIN` (`core/translate/schema.rs:2667-2676`); this task specs `CREATE TABLE` only — file follow-up tasks for TYPE/DOMAIN if desired rather than growing this one. Wording of the message ("identifiers are case-insensitive here") is illustrative; bikeshed with whoever implements.

---

### Task F4: `\q` must not bypass `rl.save_history()`

**Files:**
- Modify: `pgmicro/src/main.rs:872-925` (`consume`), `:954-962` and `:971-974` (`run_interactive`), `:985-997` (`run_stdin`)
- Test: `pgmicro/src/main.rs` (new inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: Task F1's post-fix `run_stdin()` (must land after F1 — both edit the same loop body).
- Produces: `Repl::consume(&mut self, flush: bool) -> bool` — signature change from `-> ()` to `-> bool`; any future caller of `consume` must check the return value for quit-signaling. No other task in this plan calls `consume` directly.

- [ ] **Step 1: Write the failing test**

```rust
// Add near the bottom of pgmicro/src/main.rs. This is a compile-time-enforced
// TDD red: `consume` currently returns `()`, so `assert!(should_quit)` below
// fails to typecheck until the fix lands. It's also the only safe way to
// exercise this path — the *current* code calls std::process::exit(0)
// directly inside consume(), which would kill the test process if invoked
// in-process; the fix removes that hazard by making quit an explicit,
// caller-owned return value.
#[cfg(test)]
mod tests {
    use super::*;

    fn test_repl() -> Repl {
        let (io, conn) = open_database(":memory:", None, false).expect("open test db");
        Repl::new(
            conn,
            io,
            ":memory:".to_string(),
            TableConfig::adaptive_colors(),
            Arc::new(AtomicUsize::new(0)),
        )
    }

    #[test]
    fn quit_command_signals_quit_without_killing_the_process() {
        let mut repl = test_repl();
        repl.input_buf.push_str("\\q\n");
        let should_quit = repl.consume(false);
        assert!(should_quit, "\\q must signal quit to the caller");
    }

    #[test]
    fn non_quit_command_does_not_signal_quit() {
        let mut repl = test_repl();
        repl.input_buf.push_str("\\dt\n");
        let should_quit = repl.consume(false);
        assert!(!should_quit, "\\dt must not signal quit");
    }
}
```

End-to-end behavioral coverage for `\q` itself (that it actually stops the REPL) is added separately in Task F8, using the existing `pgmicro/tests/pgmicro.rs` stdin harness.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pgmicro quit_command_signals_quit_without_killing_the_process non_quit_command_does_not_signal_quit -- --nocapture`
Expected: FAIL to compile — `consume` returns `()` today, so `let should_quit = repl.consume(false)` doesn't typecheck against `assert!(should_quit, ...)`.

- [ ] **Step 3: Implement**

```rust
// pgmicro/src/main.rs — before (lines 872-895, consume())
fn consume(&mut self, flush: bool) {
    if self.input_buf.trim().is_empty() {
        return;
    }

    let trimmed = self.input_buf.trim();

    // Backslash meta-commands
    if trimmed.starts_with('\\') {
        let input = self.input_buf.clone();
        let quit = handle_meta_command(
            input.trim(),
            &self.conn,
            &self.db_file,
            &mut self.expanded_display,
            &mut self.timing,
            &mut std::io::stdout(),
        );
        self.reset_input();
        if quit {
            std::process::exit(0);
        }
        return;
    }
```

```rust
// pgmicro/src/main.rs — after
fn consume(&mut self, flush: bool) -> bool {
    if self.input_buf.trim().is_empty() {
        return false;
    }

    let trimmed = self.input_buf.trim();

    // Backslash meta-commands
    if trimmed.starts_with('\\') {
        let input = self.input_buf.clone();
        let quit = handle_meta_command(
            input.trim(),
            &self.conn,
            &self.db_file,
            &mut self.expanded_display,
            &mut self.timing,
            &mut std::io::stdout(),
        );
        self.reset_input();
        return quit;
    }
```

```rust
// pgmicro/src/main.rs — end of consume(): before (lines 920-925)
            if had_err {
                self.had_error = true;
            }
            self.reset_input();
        }
    }
```

```rust
// pgmicro/src/main.rs — after
            if had_err {
                self.had_error = true;
            }
            self.reset_input();
        }
        false
    }
```

```rust
// pgmicro/src/main.rs — before (lines 954-962, run_interactive loop)
                Ok(line) => {
                    self.interrupt_count.store(0, Ordering::Release);
                    self.read_state.process(&line);
                    self.input_buf.push_str(&line);
                    if !self.input_buf.ends_with(char::is_whitespace) {
                        self.input_buf.push('\n');
                    }
                    self.consume(false);
                }
```

```rust
// pgmicro/src/main.rs — after
                Ok(line) => {
                    self.interrupt_count.store(0, Ordering::Release);
                    self.read_state.process(&line);
                    self.input_buf.push_str(&line);
                    if !self.input_buf.ends_with(char::is_whitespace) {
                        self.input_buf.push('\n');
                    }
                    if self.consume(false) {
                        break;
                    }
                }
```

```rust
// pgmicro/src/main.rs — before (lines 971-974)
                Err(ReadlineError::Eof) => {
                    self.consume(true);
                    break;
                }

// after: unchanged — already breaks unconditionally, consume(true)'s
// return value is irrelevant here. Left as-is intentionally.
```

The existing post-loop `let _ = rl.save_history(HISTORY_FILE.as_path());` at line 983 now runs on the `\q` path too, since it's a normal `break` instead of `process::exit`.

```rust
// pgmicro/src/main.rs — run_stdin (post-Task-F1 state), only the last
// line changes:
            self.read_state.process(&self.input_buf[prev_len..]);
            if self.consume(false) {
                break;
            }
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pgmicro quit_command_signals_quit_without_killing_the_process non_quit_command_does_not_signal_quit -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add pgmicro/src/main.rs
git commit -S -m "fix(repl): \\q breaks the REPL loop instead of process::exit, so history is saved"
```

**Notes:** **Depends on Task F1 landing first** — both modify the body of `run_stdin()`'s loop (F1 changes the `read_line` match, this changes the trailing `self.consume(false)` call into an `if`/`break`). Apply F1, then rebase this on top. `Repl` fields (`conn`, `io`, `db_file`, ...) are already `pub(crate)`-visible within the same file, so the inline unit test compiles without visibility changes.

---

### Task F5: Clear upfront error for `COPY ... FROM STDIN` / `TO STDOUT` outside `--server` mode

**Files:**
- Modify: `pgmicro/src/main.rs:682-714` (`execute_sql`), new helper added above it
- Test: `pgmicro/tests/pgmicro.rs`

**Interfaces:** None — self-contained to `pgmicro/src/main.rs`. No dependency on other Workstream F tasks.

- [ ] **Step 1: Write the failing tests**

```rust
// pgmicro/tests/pgmicro.rs

// ---------------------------------------------------------------------------
// COPY ... FROM STDIN / TO STDOUT: unsupported outside --server, must say so
// ---------------------------------------------------------------------------

#[test]
fn copy_from_stdin_repl_reports_clear_unsupported_error() {
    let output = run_pgmicro(
        b"CREATE TABLE t(id INT, name TEXT);\nCOPY t FROM STDIN;\n1\tAlice\n\\.\n",
    );
    assert_ne!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(
        out.contains("--server"),
        "expected the error to point at --server mode, got: {out}"
    );
    assert!(
        !out.to_lowercase().contains("syntax error"),
        "should not cascade into a bogus syntax error for the copy data rows, got: {out}"
    );
}

#[test]
fn copy_to_stdout_repl_reports_clear_unsupported_error() {
    let output = run_pgmicro(b"CREATE TABLE t(id INT);\nCOPY t TO STDOUT;\n");
    assert_ne!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(out.contains("--server"), "got: {out}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pgmicro copy_from_stdin_repl_reports_clear_unsupported_error copy_to_stdout_repl_reports_clear_unsupported_error -- --nocapture`
Expected: FAIL — `COPY t FROM STDIN` today falls through to a generic "COPY is handled at the connection layer" error, and the subsequent data rows get mis-parsed as bogus SQL statements, surfacing as unrelated syntax errors instead of a clear message pointing at `--server`.

- [ ] **Step 3: Implement**

```rust
// pgmicro/src/main.rs — before (lines 682-690)
fn execute_sql(
    conn: &Arc<Connection>,
    sql: &str,
    table_config: &TableConfig,
    expanded: bool,
    w: &mut dyn Write,
) -> bool {
    let runner = conn.query_runner(sql.as_bytes());
    let mut had_error = false;
```

```rust
// pgmicro/src/main.rs — after
/// Best-effort, case-insensitive detection of `COPY ... FROM STDIN` /
/// `COPY ... TO STDOUT`. The plain query API this REPL uses has no way to
/// interactively stream copy rows (that requires the PG wire protocol's
/// CopyData framing, implemented only in `cli/pg_server.rs`). Without this
/// check the backend falls through to the generic
/// "COPY is handled at the connection layer" error (core/translate/mod.rs),
/// and any data rows that follow get mis-parsed as bogus SQL statements.
fn is_unsupported_stdin_copy(sql: &str) -> bool {
    let upper = sql.trim_start().to_uppercase();
    upper.starts_with("COPY") && (upper.contains("FROM STDIN") || upper.contains("TO STDOUT"))
}

fn execute_sql(
    conn: &Arc<Connection>,
    sql: &str,
    table_config: &TableConfig,
    expanded: bool,
    w: &mut dyn Write,
) -> bool {
    if is_unsupported_stdin_copy(sql) {
        let _ = writeln!(
            w,
            "Error: COPY ... FROM STDIN / TO STDOUT is only supported over the \
             PostgreSQL wire protocol. Run pgmicro with --server and connect a PG \
             client, or use COPY ... FROM '<file>' / TO '<file>' in this REPL."
        );
        return true;
    }
    let runner = conn.query_runner(sql.as_bytes());
    let mut had_error = false;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pgmicro copy_from_stdin_repl_reports_clear_unsupported_error copy_to_stdout_repl_reports_clear_unsupported_error -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add pgmicro/src/main.rs pgmicro/tests/pgmicro.rs
git commit -S -m "fix(repl): clear upfront error for COPY FROM STDIN/TO STDOUT outside --server mode"
```

**Notes:** `execute_sql` is also used by `main()`'s single-command mode (`opts.sql`, line ~1054), so this check covers `pgmicro :memory: "COPY t FROM STDIN"` too — worth a quick manual sanity check but no separate test required (same code path).

---

### Task F6: Regenerate the stale NAPI loader version guard

**Files:**
- Modify (generated, do not hand-edit): `npm/pgmicro/index.js`
- Test: new `npm/pgmicro/index-version.test.ts`

**Interfaces:** None — packaging-only, no Rust code touched. Independent of every other task in this plan.

- [ ] **Step 1: Write the failing test**

```ts
// npm/pgmicro/index-version.test.ts
import { expect, test } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));

test("generated NAPI loader version guard matches package.json version", () => {
  const pkg = JSON.parse(readFileSync(join(here, "package.json"), "utf8"));
  const indexJs = readFileSync(join(here, "index.js"), "utf8");
  expect(
    indexJs.includes(`bindingPackageVersion !== '${pkg.version}'`),
    `index.js still checks against an old binding version; ` +
      `re-run 'npm run napi-build' in npm/pgmicro after bumping package.json ` +
      `to ${pkg.version} and commit the regenerated file.`
  ).toBe(true);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd npm/pgmicro && npx vitest run index-version.test.ts`
Expected: FAIL — `index.js` has `'0.0.3'`, `package.json` has `'0.0.5'`.

- [ ] **Step 3: Regenerate (process, not hand-edit)**

```bash
# from npm/pgmicro/, exact command already defined in package.json's scripts
# (npm/pgmicro/package.json:26):
npm run napi-build
# i.e.: npx napi build --platform --esm --features default-postgres \
#         --manifest-path ../../bindings/javascript/Cargo.toml --output-dir . \
#         && python3 ../../npm/rename-node.py .
```

Then `git diff npm/pgmicro/index.js` should show only the `bindingPackageVersion !== '0.0.3'` → `'0.0.5'` literal changes across all platform branches (no structural changes expected since no platform/target list changed). Commit the regenerated `index.js` — do not hand-edit the version strings.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd npm/pgmicro && npx vitest run index-version.test.ts`
Expected: PASS.

- [ ] **Step 5: Verify no stray native binary was staged**

Run: `git status npm/pgmicro/`
Expected: only `index.js` and `index-version.test.ts` as new/modified — `napi build` also compiles a native `.node` binary for the host platform into `npm/pgmicro/` as a side effect, and there is no `npm/pgmicro/.gitignore` today (unlike `bindings/javascript/.gitignore`, which has `*.node`). If a `.node` file appears in `git status`, do not add it — either add a local `npm/pgmicro/.gitignore` with `*.node` as a small follow-up, or `git add` only the intended files explicitly.

- [ ] **Step 6: Commit**

```bash
git add npm/pgmicro/index.js npm/pgmicro/index-version.test.ts
git commit -S -m "chore(napi): regenerate loader to match package.json version 0.0.5"
```

**Notes:**
- Assumes `napi-build` sources the embedded version from `npm/pgmicro/package.json` (consistent with the `optionalDependencies` version pins at `npm/pgmicro/package.json:40-42`, both `0.0.5`). If it instead pulls from `bindings/javascript/package.json` (currently `0.6.0-pre.24`, a different versioning lineage), the test's expected string needs adjusting — verify by actually running the command once, don't assume.
- Requires network access / the `napi` CLI + a working Rust toolchain for `bindings/javascript` — not runnable in a pure doc/text-only sandbox; whoever picks up this task needs a full dev environment, not just a text editor.

---

### Task F7: Stop routing real Intel Macs to the arm64 native binary

**Files:**
- Modify: `npm/pgmicro/cli.js` (whole file — needs a `main()` guard for safe testing)
- Test: new `npm/pgmicro/cli.test.ts`

**Interfaces:** None — packaging-only. Independent of every other task in this plan, including Task F6 (different files, `cli.js` vs `index.js`).

Confirmed no `x86_64-apple-darwin` target exists anywhere in the build: `npm/pgmicro/package.json`'s `napi.targets` (lines 46-50) lists only `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `optionalDependencies` (lines 40-42) match exactly those three. There is no evidence Intel Mac support is intended — this task removes the special case, letting genuine Intel Macs hit the same clean "unsupported platform" path as every other unbuilt target.

- [ ] **Step 1: Write the failing tests**

```ts
// npm/pgmicro/cli.test.ts
import { expect, test } from "vitest";
import { resolvePlatformPackage } from "./cli.js";

test("genuine Intel Macs are reported unsupported, not routed to the arm64 binary", () => {
  expect(resolvePlatformPackage("darwin", "x64")).toBeUndefined();
});

test("Apple Silicon and the published Linux targets still resolve", () => {
  expect(resolvePlatformPackage("darwin", "arm64")).toBe("pg-micro-darwin-arm64");
  expect(resolvePlatformPackage("linux", "x64")).toBe("pg-micro-linux-x64-gnu");
  expect(resolvePlatformPackage("linux", "arm64")).toBe("pg-micro-linux-arm64-gnu");
});
```

Note: this requires `cli.js` to be import-safe (no top-level side effects on import) — the fix below adds that guard, since `cli.js` currently executes `execFileSync`/`process.exit` as soon as it's loaded.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd npm/pgmicro && npx vitest run cli.test.ts`
Expected: FAIL to import — `cli.js` has no `resolvePlatformPackage` export today and executes side effects (`process.exit`) at module load time, which would abort the test run.

- [ ] **Step 3: Implement**

```js
// npm/pgmicro/cli.js — before (entire file)
#!/usr/bin/env node
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { chmodSync } from "node:fs";
import { join, dirname } from "node:path";

const require = createRequire(import.meta.url);

const platformPackages = {
  "darwin-arm64": "pg-micro-darwin-arm64",
  "darwin-x64": "pg-micro-darwin-arm64", // Rosetta 2
  "linux-x64": "pg-micro-linux-x64-gnu",
  "linux-arm64": "pg-micro-linux-arm64-gnu",
};

const key = `${process.platform}-${process.arch}`;
const pkg = platformPackages[key];

if (!pkg) {
  console.error(`pgmicro: unsupported platform ${key}`);
  process.exit(1);
}

let binaryPath;
try {
  const pkgJsonPath = require.resolve(`${pkg}/package.json`);
  binaryPath = join(dirname(pkgJsonPath), "pgmicro");
} catch (e) {
  console.error(`pgmicro: could not find platform package "${pkg}".`);
  console.error("Run: npm install");
  process.exit(1);
}

try {
  chmodSync(binaryPath, 0o755);
  execFileSync(binaryPath, process.argv.slice(2), { stdio: "inherit" });
} catch (e) {
  if (e.status != null) {
    process.exit(e.status);
  }
  throw e;
}
```

```js
// npm/pgmicro/cli.js — after
#!/usr/bin/env node
import { createRequire } from "node:module";
import { execFileSync } from "node:child_process";
import { chmodSync } from "node:fs";
import { join, dirname } from "node:path";

const require = createRequire(import.meta.url);

// No "darwin-x64" entry: there is no x86_64-apple-darwin build (see
// package.json's napi.targets/optionalDependencies). Real Intel Macs must
// fall through to the "unsupported platform" error below instead of being
// silently pointed at the arm64 binary (which only works under Rosetta 2,
// i.e. on Apple Silicon, not on genuine Intel hardware).
export const platformPackages = {
  "darwin-arm64": "pg-micro-darwin-arm64",
  "linux-x64": "pg-micro-linux-x64-gnu",
  "linux-arm64": "pg-micro-linux-arm64-gnu",
};

export function resolvePlatformPackage(platform, arch) {
  return platformPackages[`${platform}-${arch}`];
}

export function main(argv) {
  const pkg = resolvePlatformPackage(process.platform, process.arch);

  if (!pkg) {
    console.error(
      `pgmicro: unsupported platform ${process.platform}-${process.arch}`
    );
    process.exit(1);
  }

  let binaryPath;
  try {
    const pkgJsonPath = require.resolve(`${pkg}/package.json`);
    binaryPath = join(dirname(pkgJsonPath), "pgmicro");
  } catch (e) {
    console.error(`pgmicro: could not find platform package "${pkg}".`);
    console.error("Run: npm install");
    process.exit(1);
  }

  try {
    chmodSync(binaryPath, 0o755);
    execFileSync(binaryPath, argv, { stdio: "inherit" });
  } catch (e) {
    if (e.status != null) {
      process.exit(e.status);
    }
    throw e;
  }
}

// Only run when invoked directly (`pg-micro ...` / `node cli.js ...`), not
// when imported by tests.
if (import.meta.url === `file://${process.argv[1]}`) {
  main(process.argv.slice(2));
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd npm/pgmicro && npx vitest run cli.test.ts`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add npm/pgmicro/cli.js npm/pgmicro/cli.test.ts
git commit -S -m "fix(npm): stop silently routing genuine Intel Macs to the arm64 binary"
```

**Notes:** The `main()`/import-guard refactor is required to make the file safely importable by a test at all (today, importing `cli.js` executes it). This changes zero runtime behavior for the actual CLI invocation path (`node cli.js args` still hits the guard and runs `main()` exactly as before). If Intel Mac support turns out to be desired later, the correct fix is a different option — add `x86_64-apple-darwin` to `napi.targets`, build/publish `pg-micro-darwin-x64`, and restore a *real* (not Rosetta-aliased) `"darwin-x64"` entry — but nothing in this repo today suggests that's planned.

---

### Task F8: Add missing `\q` / `\dg` meta-command tests

**Files:**
- Modify: `pgmicro/tests/pgmicro.rs` (new tests)
- Doc (optional, not required): `EVALUATION.md`

**Interfaces:** Consumes Task F4's fixed `\q` behavior (breaks the loop instead of `process::exit`). Land after Task F4 — before F4 lands, `\q` still works from the outside (it just skips history-save), so this task's `q_quits_without_executing_further_input` test would pass even on unfixed code; landing it after F4 makes it a genuine regression guard for F4's fix rather than a coincidentally-passing test.

- [ ] **Step 1: Write the tests (this IS the deliverable — additive only, no production code change)**

```rust
// pgmicro/tests/pgmicro.rs

// ---------------------------------------------------------------------------
// Meta-commands: \q
// ---------------------------------------------------------------------------

#[test]
fn q_quits_without_executing_further_input() {
    // SELECT 1/0 is a canary: if \q failed to stop input processing, this
    // division-by-zero would surface as an "Error" in the output.
    let output = run_pgmicro(b"\\q\nSELECT 1/0;\n");
    assert_eq!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(
        !out.contains("Error"),
        "\\q should quit before executing subsequent input, got: {out}"
    );
}

// ---------------------------------------------------------------------------
// Meta-commands: \dg (alias for \du)
// ---------------------------------------------------------------------------

#[test]
fn dg_lists_roles_like_du() {
    let output = run_pgmicro(b"\\dg\n");
    assert_eq!(output.status.code(), Some(0));
    let out = stdout(&output);
    assert!(
        out.contains("turso"),
        "\\dg should list role 'turso' (alias for \\du), got: {out}"
    );
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p pgmicro q_quits_without_executing_further_input dg_lists_roles_like_du -- --nocapture`
Expected: PASS — `\q` already exits the process before this task (Task F4 only fixed the history-save side effect, not overall quit behavior), and `\dg` already works as a `\du` alias; this task closes the test-coverage gap, not a behavior gap.

- [ ] **Step 3: Commit**

```bash
git add pgmicro/tests/pgmicro.rs
git commit -S -m "test(repl): add missing \\q and \\dg coverage referenced by EVALUATION.md"
```

**Notes:** Per the original review, prefer adding tests over softening the doc claim. Once both tests exist, EVALUATION.md's "19 meta-commands, all tested" claim becomes true; touching `EVALUATION.md` itself is optional and out of scope for this code-focused task — flag it to whoever owns doc upkeep rather than editing it here.

---

### Workstream F sequencing notes

1. **Strict chain:** Task F1 (H33) → Task F4 (`\q` history-save) → Task F8 (`\q`/`\dg` tests). F1 and F4 both edit `run_stdin()`'s loop body on adjacent lines — F1 must land first and F4 rebases on top. F8 should land after F4 so its `\q` test is a genuine regression guard for F4's fix, not a coincidentally-passing pre-existing-behavior test.
2. Task F2 (H34a, quote stripping) is fully independent — different function (`handle_meta_command`'s `\d`/`\d+` arms) — can land anytime, any order relative to the F1→F4→F8 chain.
3. Task F3 (H34b, `core/` error message) is fully independent — different file (`core/translate/schema.rs`) entirely. Flag for extra review since it's a `core/` change; land whenever bandwidth allows.
4. Task F5 (COPY STDIN upfront error) is fully independent — different function (`execute_sql`) — can land anytime.
5. Tasks F6 and F7 are npm-packaging-only, touch different files (`index.js` vs `cli.js`), and have zero dependency on any Rust-side task in this plan or on each other — assign to whoever has npm/JS tooling available, any order.

---

