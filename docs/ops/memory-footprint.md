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

### 3.6 Where a fresh process puts its memory (`component_breakdown_probe`)

One process on a copy of a 759 MB database, pool cap 384 MB, real embedder, four namespaces:

| Step | Resident memory |
|---|---|
| Process start | 12 MB |
| Store open | 47 to 59 MB |
| First vector search, every namespace | 517 to 567 MB |
| Exact-name search, every namespace | +4 to +45 MB |
| Embedder loaded | 1,017 to 1,036 MB (peak 1,540 to 1,558 MB while loading) |
| Second search pass | 1,137 to 1,150 MB |
| After `HeapCompact` on all 4 process heaps | no change |
| After `EmptyWorkingSet` | 0 MB |
| One search pass after that | 553 MB |

- The first search fills the buffer pool (about 320 to 440 MB), then stays flat. Growth on a first search is the pool filling, not a leak.
- `HeapCompact` freed nothing, so the retained memory is not allocator slack.
- About 590 MB of the 1.14 GB is resident but idle: the embedder weights, which only embed calls touch, and cold pool pages. A search pass after trimming brought back only 553 MB, and took about 12% longer.
- A long-running daemon sits roughly 500 MB above this (1.66 GB against 1.14 GB after a similar search). That gap is not explained yet. Task `neurostrata-yl3o`.
- Trimming the idle working set lowers resident memory, not committed memory: the pages move to the standby list. Unloading the embedder is what lowers committed memory. Tasks `neurostrata-chhm` and `neurostrata-00kg`.

### 3.7 The hygiene repair (`hygiene_writer_wait_probe`)

`repair_namespace_hygiene` used to scan and repair the whole namespace in one write turn. On a large namespace it outlived the 30 second write deadline (the daemon logged `gave up waiting on repairing namespace hygiene`) and every other writer queued behind it.

| Version | Repair time on the 759 MB copy |
|---|---|
| One write turn for the whole namespace | over 30 s, deadline hit on the live daemon |
| One write turn per row (a keyed read per row, a lookup per edge target) | 193 s; the writer waited at most 460 ms |
| Id-ordered pages, targets resolved once, existing edges read once, plan in Rust, only changes written in chunks of 40 | 2.5 s; live runs took about 3 s and 4 s on two namespaces, and a rerun found nothing to fix |

Things to keep when changing it:

- Readers and writers share one lock (`with_conn` takes the writer's turn), so a long read blocks writers too. Keep every turn short.
- Look rows up by `m.id = '...'`: it is a hash-indexed primary-key lookup (about 0.4 ms in a 20,000-row probe). `m.id IN [...]`, an `OR` across `id` and `namespace`, and `STARTS WITH` do not use that index; `EXPLAIN` shows a full `SCAN_NODE_TABLE` with a filter (1.6 to 2 ms there). A scan is cheap while the query returns small columns. The large memory cost in section 1 comes from also reading `m.embedding`, which the repair never does.
- The candidate pass only chooses rows and writes nothing. Each chunk of candidates is repaired in one write turn that re-reads the rows and everything they are planned from, with keyed lookups, so a plan is never older than its turn. The turn holds the store's single lock (`with_conn` and `write_with_deadline` share it) and the database file lock keeps other processes out. If a read in the turn fails, nothing is written. A row that stopped needing repair is skipped; one that started needing it waits for the next run.
- Timings of the current version, single runs on a copy of the live database made with `neurostrata-mcp backup` (`hygiene_writer_wait_probe`, one process per run). The largest namespace: 4.2 s with 4 shape and 3 dangling fixes, then 4.1 s with none; the previous version with per-row rechecks took 4.3 s and 4.1 s with the same result. A writer to another namespace waited 0.5 s at most in both. Almost all of that time is the read-only candidate pass, because only 7 rows needed repair. With 3,000 synthetic rows that all needed repair (an empty temp store, 4-float embeddings): 14.2 s for the current version and 26.3 s for the previous one, and a second pass took 0.3 s. Not measured: repeated runs, a cold file cache, or a namespace larger than the copy's.
- The repair archives refs that resolve to nothing into `metadata.archived_refs`. It does not relink them.
- The automatic run fires only when `CARGO_PKG_VERSION` changes (the doctor check in `src/server.rs`). Redeploying the same version does not run it. Use `neurostrata-mcp repair-hygiene <namespace>`, which needs the daemon stopped.

### 3.8 Tried and not kept (do not retry without new evidence)

All measured on a copy of the database with one process per variant.

- **Push the exact-name match into the query** (`lower(location) = t OR lower(content) = t OR lower(metadata) CONTAINS t`, Rust checks kept on the survivors). Exact-name stage: 3.8 s and +42 MB with it, 3.1 s and +4 MB without. A live `search_memory` on the largest namespace grew +615 MB with it deployed, against +649 MB before. The exact-name pass was not the cost. Reverted.
- **Rank on id and distance only, then fetch the winners by id** (`ids_first`). For one cold namespace: +318 MB and a peak of 397 MB, against +440 MB and 547 MB for the current query, at the same warm latency. Across all namespaces the peak was 555 to 570 MB either way, because the shared pool bounds it. Not kept.
- **Page the embeddings into Rust and rank there** for `search_memory` (1,000 rows per page). +447 MB and 7.8 s per pass against 0.2 to 0.6 s: about 40 times slower, no memory saved. This works for the guard, which ranks a handful of rules. It does not work for search.
- **Judging a search by its first call.** A cold search includes the pool filling to its cap. Measure the second call, or growth beyond the pool cap.

### 3.9 Listing the whole namespace to find a few rows (`task_list_variants_probe`)

`list(ns, None, None)` reads every row of a namespace, content and metadata included, and a handler that then keeps a handful of them pays for all of it on every call. One process per variant, on a copy of the database:

| Query | Rows | First call | Each later call | Time | Peak |
|---|---|---|---|---|---|
| Whole namespace (8,140 rows) | 8,140 | +272 MB | +93 to +99 MB | 324 to 509 ms | 325 MB |
| `memory_type = 'task'` | 47 | +127 MB | +1 to +4 MB | 48 to 202 ms | 177 MB |
| Whole namespace (34,374 rows) | 34,374 | +278 MB | +108 to +116 MB | 287 to 424 ms | 351 MB |
| `memory_type = 'task'` | 0 | +123 MB | +2 MB | 47 to 196 ms | 190 MB |
| `metadata CONTAINS 'pinned'` (22 rows) | 22 | +119 MB | +3 to +6 MB | 48 ms | 194 MB |

The first-call figure is the buffer pool filling and is the same for every query. The difference is what each later call costs. Three callers used the whole-namespace listing and now ask for what they use:

- `neurostrata_task_list` lists task rows only (blocker statuses read tasks and nothing else).
- `record_graph_query`, which runs on every graph-first lookup (`memories_for_file` and the related tools), lists task rows, reads the registry row by its fixed id, and lists directory rows only when the registry row has no `project_root`. When both a registry root and a git directory exist, the registry root now wins; before, it was whichever row came first.
- `collect_golden_rules` asks for `VectorStore::list_pinned` for the project namespace. The default implementation lists everything, so other stores behave as before; `LadybugStore` narrows with `metadata CONTAINS 'pinned'` and returns a superset that the caller still filters.

Still listing the whole namespace, because each needs more than the task or pinned rows: `task_complete` (looks for an extraction edge among all memories), id allocation in `task_create`, `gate::run`, the `global` namespace in `collect_golden_rules`, and several `server.rs` handlers.

### 3.10 Ingest read the whole namespace twice to use one generation (`task_list_variants_probe`)

`ingest_directory` reads the previous generation twice: once to decide whether anything changed (file hashes, version stamps, node paths) and once to carry unchanged rows forward. Both reads called `list(ns, None, None)` and then threw away every row not in that generation. `list` returns each reachable row with its full text, and it also returns every row written before generations existed (their ids have no generation segment).

Ingest now calls `VectorStore::generation_summary(ns, generation)`. `LadybugStore` narrows by id prefix in the engine and returns only `id`, `memory_type`, `location`, `file_sha256` and `ingest_version` per row (`GenerationRow`); it never fetches `content` and drops each row's metadata after reading those fields. It applies the same reachability rule as `list` (`ActiveGenerations::reaches`). The default implementation lists the namespace, as before, so other stores behave as they did.

One process per variant, on a copy of the database, the namespace holding 7,190 rows in its active generation:

| Read | Rows returned | First call | Each later call | Peak |
|---|---|---|---|---|
| `list(ns, None, None)` (what ingest did, per read) | 8,288 | +271 MB | +98 MB | 340 MB |
| `generation_summary(ns, active generation)` | 7,190 | +133 to +135 MB | +1 to +3 MB | 188 to 206 MB |

The first call is the buffer pool filling and is the same for every query. A second namespace of 34,382 rows was written before generations: none of its ids are generation-scoped. Ingest does not read it at all today (`prev_gen` is 0), and after its first generation ingest the old code would have listed those rows on every later ingest; the new read never fetches them because the id prefix excludes them. That part is reasoned from the ids, not measured end to end.

The whole run barely moves (`ingest_reads_probe`, this repo's `src` directory copied to a temp tree, 166 files, zero embedder, 384 MB pool, one file added before each pass from the second on; one run per code state). Highest resident set inside each pass:

| Pass | `list` (before) | `generation_summary` (after) |
|---|---|---|
| 1 (first ingest, nothing to read) | 380 MB | 375 MB |
| 2 | 421 MB | 394 MB |
| 3 | 518 MB | 487 MB |
| 4 | 543 MB | 535 MB |
| Process peak | 562 MB | 567 MB |

So in a real ingest the saving is about 10 to 30 MB at this size and the process peak is the same within noise. The isolated read figures above overstate it: during an ingest the buffer pool is already full from the writes, so the listing does not add its 100 MB on top. Passes 3 and 4 also ran 25 to 35 s faster (146 to 162 s against 163 to 192 s), from one run each. Not measured: a larger tree, where the listing grows with the row count, or the real embedder.

What does grow is the peak itself, by roughly 50 to 100 MB per pass in both columns (380, 421, 518, 543 MB before). The reads are not the cause. Older generations stay in the store and each changed pass copies every unchanged row forward (task `neurostrata-bcui`), which is the likelier source. The embedder (about 1 GB to load) is still the larger share of an ingest (section 3.6).

### 3.11 Superseded generations pile up (`task_list_variants_probe`, `ingest_reads_probe`)

Every ingest that changes anything copies each unchanged row into a new generation and keeps the old one as history. Nothing prunes. `list` and search hide superseded rows, so the cost does not show in the row counts a caller sees.

On a copy of the database (`NS_PROBE_VARIANT=generations`, ids only):

| Namespace | Rows in the store | Reachable through `list` | Rows per generation |
|---|---|---|---|
| NeuroStrata | 17,458 | 8,288 | 1: 1,980; 2: 7,190; 3: 7,190 (active); 1,098 unscoped |
| A second, written before generations | 34,382 | 34,382 | none scoped |

So 9,170 of the first namespace's rows (52%) are superseded copies. The second has none.

`ingest_reads_probe` on a 166-file tree (zero embedder, 384 MB pool, one file added before each pass, no periodic checkpoint):

| Pass | Highest RSS in the pass | Time |
|---|---|---|
| 1 (first ingest) | 362 to 392 MB | 122 to 126 s, all in walk and stage (about 27 ms per row) |
| 2 | 401 to 402 MB | 139 to 141 s |
| 3 | 465 to 485 MB | 138 to 140 s |
| 4 | 518 to 553 MB | 142 to 156 s |
| 5 | 581 to 602 MB | 143 to 147 s |

- About 99% of each changed pass is the carry-forward (4,409 to 4,415 nodes at about 36 ms each, one `get` and one `upsert` per node). Walk and stage took 0.6 s once the files were unchanged.
- The highest point was still rising at pass 5.
- The probe's database file was 2.3 GB after five passes, against 797 MB for the live database.
- During pass 6 writes began to time out (245 `gave up waiting on writing a memory`, each caller waiting its 30 s) while the engine reported 0 calls in flight, and the run was stopped. An earlier 8-pass run failed the same way at pass 6: the ingest returned an error after carrying only 1,196 of about 4,417 nodes.

What this establishes: the growth per pass is not the listing reads (section 3.10) and not the embedder (a zero embedder was used). What it does not establish:

- The cause of the write stall. The probe does not run the daemon's 2-second periodic checkpoint, so the log may simply grow until one checkpoint outlasts the deadline. Whether a daemon can hit it is not known.
- That superseded generations cause the per-pass growth. That is the explanation left once the reads and the embedder are ruled out. A run that prunes old generations after publish would test it. Pruning changes the design that keeps history (task `neurostrata-bcui` lists the open questions), so it needs the maintainer's decision.
- Whether the probe's 2.3 GB is real or inflated by the missing checkpoints.

---

## 4. Not fixed yet

- `neurostrata_task_list` (+197 MB) and `neurostrata_task_claim` (+107 MB) spike the same way on a fresh daemon. Task `neurostrata-zoz2`.
- Whether the 384 MB cap brings the running daemon's steady state down as much as the probe suggests is not measured on a live daemon yet. Compare `memory-growth.log` before and after the restart.
- `rules_from_rows` in `src/guard/semantic.rs` used to keep rows whose score was at least 0.25 and call that a similarity floor, but store scores are distances (lower is closer). It now keeps rows at distance 0.75 or less (similarity 0.25 or more, `similarity = 1 - distance`), and a listed guard rule gets distance 0.0. Task `neurostrata-rhnz`.

---

## 5. Watching it on your own daemon

`neurostrata_storage_status` returns a `process` object with `rss_mb` and `peak_rss_mb`.

The daemon also appends JSON lines to `~/.config/neurostrata/memory-growth.log`:

- `kind: "call"`: a tool call that left the process at least `NEUROSTRATA_RSS_LOG_MB` larger (default 50). It records the tool, the namespace, before and after, and `in_flight`. When another call was running, the line carries `"approximate": true` and the growth may belong to the neighbor.
- `kind: "sample"`: one row every `NEUROSTRATA_RSS_SAMPLE_SECS` (default 600, 0 turns it off).

The call log and the sampler are **off by default**. Turn them on with `neurostrata-mcp daemon --profile`, or with `NEUROSTRATA_PROFILE=1` (also `true` or `on`) for a daemon that an IDE starts on its own. The `rss_mb` figure in `storage_status` and the advice level are always on, because they are one syscall. The log rolls over at 4 MB (`memory-growth.log.1`). The measured cost is about 210 ns per sample and 727 ns per call.

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

# where a fresh process puts its memory, with trimming stages (NS_PROBE_POOL_MB defaults to 384)
NS_PROBE_DB=<dir> cargo test --release --bin neurostrata-mcp component_breakdown_probe -- --ignored --nocapture

# how long the hygiene repair takes and how long a concurrent writer waits (NS_PROBE_NS is the namespace)
NS_PROBE_DB=<dir> NS_PROBE_NS=<namespace> cargo test --release --bin neurostrata-mcp hygiene_writer_wait_probe -- --ignored --nocapture

# whole-namespace listing against narrower reads: NS_PROBE_VARIANT is all, tasks, pinned, ingest_nodes,
# generation_summary (what ingest reads), shape (row counts by type, no content), generations (rows per
# generation, ids only: how much is superseded) or relink_full / relink_lean (the read relink_edges does)
# a whole ingest with a generation already in the store, highest resident set per pass (NS_PROBE_DIR is copied; ~10 min for 4 passes):
#   NS_PROBE_DIR=<dir> NS_PROBE_PASSES=4 cargo test --release --bin neurostrata-mcp ingest_reads_probe -- --ignored --nocapture
NS_PROBE_DB=<dir> NS_PROBE_NS=<namespace> NS_PROBE_VARIANT=tasks cargo test --release --bin neurostrata-mcp task_list_variants_probe -- --ignored --nocapture
```

On a live daemon, call a tool once with nothing else running, then read the last lines of `memory-growth.log`. A call under the threshold writes no line.
