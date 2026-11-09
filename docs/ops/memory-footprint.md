# Daemon Memory Footprint

> **Read this before you write a query that touches `m.embedding`.** On the live database, a distance query over the Memory table cost 120 to 300 MB of resident memory per call, whatever filter it carried. The guard ran one before every edit.

This page records what we measured when the shared daemon grew from about 850 MB to over 3 GB in an hour of normal IDE use, what changed because of it, and how to measure it again. All numbers are from 2026-11-09 on a Windows release build, against a copy of the live database (739 MB, about 42,000 rows across three namespaces, one of them far larger than the others).

---

## 1. The rule

Do not use either of these on the `Memory` table when you only want a handful of rows:

```cypher
-- reads the whole embedding column into the buffer pool
MATCH (m:Memory) WHERE m.namespace = 'x' AND m.memory_type IN ['rule']
RETURN m.id, array_distance(m.embedding, [..]) AS dist ORDER BY dist LIMIT 3

-- same cost, even though it names the rows
MATCH (m:Memory) WHERE m.id IN ['a', 'b', 'c']
RETURN m.id, array_distance(m.embedding, [..]) AS dist
```

Do this instead (see `search_by_types` in `src/store/ladybug.rs`):

1. List the qualifying ids without the embedding column: `MATCH (m:Memory) WHERE m.namespace = 'x' AND m.memory_type IN [..] RETURN m.id`. If nothing comes back, stop. No vector work is needed.
2. Read each row with a keyed lookup: `MATCH (m:Memory) WHERE m.id = 'a' RETURN m.embedding, ..`.
3. Rank the few rows in Rust.

The cause is inferred from the measurements in section 3. We did not inspect the engine's query plan.

---

## 2. What changed

| Change | Where | Effect |
|---|---|---|
| `guard_validate` lists only task rows for the Zero-Action Start check | `handle_guard_validate` in `src/server.rs` | It used to list every row of the namespace (AST symbols, doc sections) on each mutating call. |
| `VectorStore::search_by_types`: a typed vector lookup with no graph expansion | `src/traits.rs`, `src/store/ladybug.rs` | The guard asks for `rule` and `guard_rule` rows only, in its own namespace and `global`. |
| The text the guard embeds is capped at 2000 characters | `src/guard/semantic.rs` | Trigger matching still sees the full payload. |
| Process memory reporting and a growth log | `src/procmem.rs` | See section 5. |

`search_by_types` also means a rule can no longer be crowded out of the top few results by non-rule rows.

---

## 3. Measurements

### 3.1 The guard, on the running daemon

Three solo `guard_validate` calls (nothing else running), in the `NeuroStrata` namespace:

| Build | Call 1 | Call 2 | Call 3 |
|---|---|---|---|
| Before any change (old daemon's log; some calls overlapped, so these are upper bounds) | +239 to +615 MB | | |
| `list()` fix only | +195 MB | +176 MB | +104 MB |
| Plus `search_by_types` over one distance query | +286 MB | +125 MB | +121 MB |
| Plus keyed lookups and Rust ranking | +53 MB | under 50 MB | under 50 MB |

"Under 50 MB" means the call wrote no line to the growth log (its threshold is 50 MB).

### 3.2 The store reads, in isolation (`guard_scan_probe`)

One process runs the steps in this order, starting at 98 MB, so later rows start from a warmer cache. Each variant of the search was measured in a separate run of the probe.

| Step | Resident memory | Time |
|---|---|---|
| `list` task rows, `NeuroStrata` | 98 to 222 MB (first query in a process) | 179 ms |
| One distance query, `NeuroStrata`, 3 rule rows | 224 to 500-524 MB (two runs) | 0.8 to 1.3 s |
| One distance query, `global`, 0 rule rows | +120 MB | 171 ms |
| Ids first, keyed reads, Rust ranking, `NeuroStrata` | 225 to 223 MB | 119 ms |
| Ids first, `global` (no rows, returns early) | no change | 10 ms |

### 3.3 Ranking in the engine or in Rust (`guard_db_rank_probe`)

48 rule rows, one process per variant:

| Variant | Resident memory | Time | Nearest distance |
|---|---|---|---|
| Keyed read, rank in Rust | 98 to 121 MB | 191 ms | 1.7026899 |
| Keyed read, `array_distance` in the engine | 98 to 132 MB | 1962 ms | 1.7026901 |
| Keyed with `namespace AND id`, engine | 98 to 132 MB | 2349 ms | 1.7026901 |
| One `m.id IN [...]` query, engine | 98 to 394 MB | 1693 ms | 1.7026901 |

The Rust distance is Euclidean and agrees with the engine's `array_distance` to within 2e-7. The engine can rank without the memory cost if the lookup is per row, but it is about ten times slower, and the Edit hook gives the guard 5 seconds.

### 3.4 The embedder (`embed_call_probe`)

Not the cause. With the real model loaded (574 MB), repeated short and 2000-character embeds moved resident memory by 0 to 2 MB each. Loading the model once peaks at 1094 MB.

### 3.5 The buffer pool cap (`pool_curve_probe`)

The engine's buffer pool caches database pages and, left alone, is sized to most of the machine's memory. After one big search the running daemon went from about 860 MB to about 1.9 GB and stayed there. We capped the pool and measured peak memory and query time on a copy of the database. Each cap ran in its own process: three passes over every namespace, each pass a general search, a task list and a typed search per namespace. No cap produced an error.

| Pool cap | Peak memory | Resident at the end | Warm pass time |
|---|---|---|---|
| engine default | about 700 MB | about 580 MB | 3.2 to 4.5 s |
| 384 MB | 583 to 607 MB | 450 to 475 MB | 3.4 to 4.2 s |
| 320 MB | 565 to 575 MB | 380 to 455 MB | 4.1 to 4.5 s |
| 256 MB | 506 MB | 373 MB | 4.8 to 5.0 s |
| 192 MB | 449 MB | 324 MB | 5.6 to 5.9 s |

Run-to-run noise is large: the default alone ranged from 3.2 to 4.5 s per warm pass across runs, so differences under about 20% are not real. Memory figures were stable. In an earlier run at 64 MB, 8 of 12 queries failed with "The buffer pool is full".

The default is **384 MB**: its speed is indistinguishable from the engine default and it saves about 100 MB of peak memory on this database. Below 320 MB the slowdown is real. A bigger database or a heavier workload needs more pool for the same speed, so the cap is a setting:

- `storage_hygiene.buffer_pool_mb` in `~/.config/neurostrata/config.json`. Default 384. `0` hands the sizing back to the engine.
- `NEUROSTRATA_BUFFER_POOL_MB` overrides the file when it parses.
- Anything under 128 MB is raised to 128 MB and a warning is printed, because a pool too small for one query fails that query.
- A pool-full failure now says which setting to raise.

The daemon prints the pool size it runs with when it starts.

---

## 4. Not fixed yet

- `neurostrata_task_list` (+197 MB) and `neurostrata_task_claim` (+107 MB) spike the same way on a fresh daemon. Task `neurostrata-zoz2`.
- Whether the 384 MB cap brings the running daemon's steady state down as much as the probe suggests is not measured on a live daemon yet. Compare `memory-growth.log` before and after the restart.
- `rules_from_rows` in `src/guard/semantic.rs` keeps rows whose score is at least 0.25 and calls that a similarity floor, but store scores are distances. Task `neurostrata-rhnz`. Behavior is unchanged.

---

## 5. Watching it on your own daemon

`neurostrata_storage_status` returns a `process` object with `rss_mb` and `peak_rss_mb`.

The daemon also appends JSON lines to `~/.config/neurostrata/memory-growth.log`:

- `kind: "call"`: a tool call that left the process at least `NEUROSTRATA_RSS_LOG_MB` larger (default 50). It records the tool, the namespace, before and after, and `in_flight`. When another call was running, the line carries `"approximate": true` and the growth may belong to the neighbor.
- `kind: "sample"`: one row every `NEUROSTRATA_RSS_SAMPLE_SECS` (default 600, 0 turns it off).

`NEUROSTRATA_MEMLOG=0` turns off both the call log and the sampler. The `rss_mb` figure in `storage_status` stays. The log rolls over at 4 MB (`memory-growth.log.1`). The measured cost is about 210 ns per sample and 727 ns per call.

---

## 5b. Telling the agent (and through it, you)

The daemon tells the agent when memory needs a human decision. The agent is never told to restart anything itself.

- **`neurostrata_storage_status`** returns `process.advice`: `level` (`ok`, `watch` or `act`), the `reason`, the pool-full failures counted so far, the thresholds, and `suggestions` (empty at `ok`).
- **`neurostrata_get_snapshot`**, which agents read first, starts with a `DAEMON MEMORY ADVISORY` block when the level is `watch` or `act`, and nothing when it is `ok`.
- **A pool-full failure** now starts with `NEUROSTRATA_ADVICE[buffer_pool]`, counts toward the level, and tells the agent to ask you first. The plugin core (OpenCode, Claude Code, Codex, GitHub Copilot CLI) adds a short reminder to such a result, at most once per session every ten minutes.

| Level | When | What the agent is told |
|---|---|---|
| `ok` | memory under the watch threshold, no pool failures | nothing |
| `watch` | resident memory at or above `NEUROSTRATA_RSS_WATCH_MB` (default 1536) | mention it if it matters; nothing is failing |
| `act` | at or above `NEUROSTRATA_RSS_ACT_MB` (default 2560), or any pool-full failure | tell the user, ask before acting, offer the config or env change and the restart steps |

The restart is always yours: `neurostrata-mcp shutdown` (graceful, can take minutes), after which the next MCP connection starts a new daemon. It disconnects every IDE sharing the daemon, and killing the process loses memories that are not yet checkpointed, so the agent is told never to do either.

The thresholds are starting points: the daemon idles near 860 MB and, before the pool cap, climbed to about 1.9 GB after one large search. Each change of level also writes an `advice` line to `memory-growth.log`, so you can see when it fired and tune the two numbers.

---

## 6. Measure it again

The probes are ignored tests in `src/procmem.rs`. Run them against a **copy** of the database, never the live one, and run each variant in its own process because the page cache does not shrink.

```bash
# store reads (point NS_PROBE_DB at a folder holding a copy of ladybug.db)
NS_PROBE_DB=<dir> cargo test --release --bin neurostrata-mcp guard_scan_probe -- --ignored --nocapture

# engine ranking against Rust ranking: NS_PROBE_VARIANT is rust, eq, eq_ns or in
NS_PROBE_DB=<dir> NS_PROBE_VARIANT=eq cargo test --release --bin neurostrata-mcp guard_db_rank_probe -- --ignored --nocapture

# cost of one embed call with the real model (needs the model in the shared cache)
cargo test --release --bin neurostrata-mcp embed_call_probe -- --ignored --nocapture
```

On a live daemon, call a tool once with nothing else running, then read the last lines of `memory-growth.log`. A call under the threshold writes no line.
