# Design: pgGraph Integration (Plan Only)

**Status:** Evaluation complete — no implementation  
**Evaluated:** [pgGraph v0.1.7](https://github.com/Evokoa/pgGraph) (June 2026)  
**Related:** `perf/graph-queries/` (relational graph benchmark baseline in pgmicro)

## Summary

pgGraph is a **pgrx PostgreSQL extension** (Rust compiled into the Postgres server) that
builds a read-optimized CSR graph projection over existing relational tables. It exposes
traversal, shortest-path, search, and a partial GQL subset via SQL functions in the `graph`
schema.

**pgmicro cannot `CREATE EXTENSION graph`.** pgmicro is Turso/SQLite under a PG wire
front-end — not a Postgres backend with shared memory, SPI, background workers, mmap of
`$PGDATA`, or extension loading. Direct extension install is impossible.

Integration therefore means one of three strategies: **sidecar Postgres**, **engine port**,
or **Turso-native graph subset**. This document evaluates each and proposes a phased plan.

## What pgGraph Is

| Layer | Technology |
|-------|------------|
| Public API | SQL functions: `graph.traverse()`, `graph.search()`, `graph.build()`, `graph.gql()`, etc. |
| Runtime | Per-backend `Engine` in thread-local storage (no shared Rust heap across connections) |
| Storage | Source tables remain authoritative; derived `.pggraph` mmap artifacts beside `$PGDATA` |
| Topology | Forward CSR + materialized reverse CSR; filter indexes; resolution index (table+PK → node) |
| Sync | Trigger-based `graph._sync_log`, projection segments for `mutable_overlay`, maintenance rebuild |
| Build | SPI cursor batches over registered tables; background workers for async build/maintenance |

Source tables, ACLs, and backups stay in PostgreSQL. pgGraph is an acceleration layer, not
a second source of truth.

## pgmicro Baseline

pgmicro already has `perf/graph-queries/` — a relational schema (nodes/edges as tables) with
SQL traversal benchmarks against Turso. That path uses recursive CTEs / joins, not a CSR
engine. pgGraph would replace or complement that workload with bounded BFS/shortest-path
primitives if ported.

## Integration Options

### Option A: Sidecar Postgres (lowest risk, dual database)

Run real PostgreSQL alongside pgmicro. Application or sync layer keeps source tables
consistent; pgGraph runs on the Postgres side.

```
App ──PG wire──► pgmicro (OLTP, embedded)
  │
  └──PG wire──► PostgreSQL + pgGraph (graph queries)
```

| Pros | Cons |
|------|------|
| Full pgGraph feature set immediately | Two databases, sync complexity |
| No engine port | Not "in-process" pgmicro |
| Matches pgGraph's design assumptions | Cross-DB joins impossible |

**Fit:** Teams already running Postgres for graph analytics; pgmicro for embedded edge/CI.

### Option B: Engine Port (highest fidelity, largest effort)

Extract the `graph` crate from pgGraph (CSR stores, traversal, persistence format) and
host it inside Turso as:

- A loadable extension (Turso extension API), or
- Built-in virtual tables / scalar functions, or
- A separate native library called from `core/functions/postgres.rs`

Replace SPI hydration with reads from Turso B-trees; replace mmap `$PGDATA` paths with
Turso file I/O; replace background workers with async Turso tasks or explicit
`graph.build()` calls.

| Pros | Cons |
|------|------|
| Single embedded database | Large port: registration, sync, ACL, persistence |
| Matches pgGraph semantics | pgrx/SPI/pg-specific code must be rewritten |
| Reuses CSR + algorithm investment | Two-plan rule: Turso graph engine first, pgmicro wrap second |

**Estimated scope:** Multi-month. pgGraph's contributor docs list ~15 subsystems (builder,
persistence, sync, sql_facade, safety, GQL planner).

### Option C: Turso-Native Graph Subset (pragmatic v1)

Implement a minimal graph layer in Turso without full pgGraph parity:

1. `graph.register_table()` catalog tables (virtual or persistent)
2. `graph.build()` — scan user tables, build in-memory CSR (no mmap v1)
3. `graph.traverse(seed, depth)` — bounded BFS returning table/PK coordinates
4. Defer: GQL, mutable overlay, trigger sync, WAL mode, shortest-path, full-text search

| Pros | Cons |
|------|------|
| Fits pgmicro architecture | Not pgGraph-compatible API |
| Builds on existing `perf/graph-queries` patterns | Users expecting `CREATE EXTENSION graph` will be surprised |
| Smallest core diff if designed as Turso feature | May duplicate pgGraph roadmap |

**Recommended as first pgmicro-native step** if graph is a product requirement and sidecar
Postgres is unacceptable.

## pgGraph → pgmicro Gap Analysis

| pgGraph capability | pgmicro today | Gap severity |
|--------------------|---------------|--------------|
| `CREATE EXTENSION graph` | N/A (not Postgres) | **Blocker** for direct use |
| `regclass` / OID table refs | `regclass` stubbed as INTEGER | **High** — registration uses table OIDs |
| FK-based edge auto-discovery | FK metadata in `pg_constraint` (partial) | **Medium** |
| SPI hydration (source row fetch) | Turso table scan | **Medium** — different API, same data |
| Background workers (async build) | No background workers | **High** — need sync build or Turso job queue |
| Trigger sync (`graph._sync_log`) | No trigger execution on PG triggers | **High** |
| mmap `.pggraph` persistence | SQLite B-tree / Turso WAL | **Medium** — port persistence format or redesign |
| `graph.gql()` write path | No GQL | **High** (deferred in pgGraph too for many writes) |
| jsonb filters in traversal | jsonb type exists; wire/filter parity partial | **Medium** |
| RLS / ACL on graph functions | Single `turso` role | **Medium** |
| `graph.enabled` GUC | PRAGMA subset only | **Low** — map to PRAGMA |
| Custom-type WHERE (interval, etc.) | Known panic on some custom-type comparisons | **High** for mixed workloads |

## Recommended Phased Plan

### Phase G0: Decision + API contract (1 week, docs only)

- [ ] Choose Option A, B, or C with product owner
- [ ] If B or C: publish Turso-native graph design (two-plan rule — no "postgres" in core doc)
- [ ] Define compatibility target: pgGraph SQL API subset vs new `turso_graph_*` API

### Phase G1: Sidecar validation (Option A, ~2 weeks)

- [ ] Docker Compose: Postgres 16 + pgGraph + pgmicro
- [ ] Replicate `perf/graph-queries` schema in both engines
- [ ] Compare traversal latency and correctness
- [ ] Document sync approach (logical replication, CDC, or app-level dual-write)

**Exit criteria:** Demonstrate pgGraph value on same dataset pgmicro uses today.

### Phase G2: Turso graph MVP (Option C, ~4–6 weeks)

| PR | Scope |
|----|-------|
| G2-1 | Catalog: `graph._registered_tables`, `_registered_edges` virtual tables |
| G2-2 | In-memory CSR build from registered tables |
| G2-3 | `graph_traverse(seed_table, seed_pk, depth)` scalar/table function |
| G2-4 | pgmicro: translator passes through `graph.*` calls; catalog stubs for psql |
| G2-5 | Tests from `perf/graph-queries` queries |

**Non-goals for G2:** GQL, mmap persistence, trigger sync, background workers.

### Phase G3: Convergence with pgGraph (Option B, long-term)

- [ ] Evaluate upstreaming Turso graph core to pgGraph or sharing `graph` crate
- [ ] Persistence format alignment (`.pggraph` import/export)
- [ ] SPI → VDBE scan adapter
- [ ] Partial `graph.gql()` if PG pattern hooks stabilize

## Key Decisions

1. **No `CREATE EXTENSION` illusion** — pgmicro should not fake extension loading; expose
   graph via functions and catalog tables with clear docs.
2. **Two-plan rule** — any CSR engine in `core/` must be justified as a Turso feature;
   pgmicro layer adds PG naming and catalog only.
3. **Source tables authoritative** — match pgGraph principle; graph is derived state.
4. **Start with Option A or C** — Option B only if embedded single-process graph is a hard
   requirement and G2 API proves insufficient.
5. **Fix custom-type WHERE panic before graph** — graph hydration and filters will hit the
   same VDBE comparison bugs as interval/money workloads.

## Open Questions

1. Does the product need **pgGraph API compatibility** or just **graph traversal on PG SQL**?
2. Is **embedded single-file** (pgmicro `:memory:` / one `.db`) mandatory, ruling out Option A?
3. Should graph registration use **table names** (pgmicro-friendly) instead of **regclass OIDs**?
4. **Upstream relationship:** contribute Turso graph core to pgGraph vs maintain a fork?

## References

- pgGraph repo: https://github.com/Evokoa/pgGraph
- Architecture: https://docs.evokoa.com/pggraph/contributor_guide/architecture
- Limitations: https://docs.evokoa.com/pggraph/user_guide/limitations-and-fit
- SQL API: https://docs.evokoa.com/pggraph/user_guide/api-reference
- pgmicro relational graph bench: `perf/graph-queries/`