# Where the memory goes — 2026-11-09 — the attribution

**The question**: the peak did not come down as much as the work promised. Where
does the peak actually go?

**The ruler**: the server's own `/api/heartbeat` (it reports DuckDB's ledger, the
queue's depth and the process's RSS) sampled every 100 ms, plus `/proc/<pid>/status`
and `/proc/<proc>/smaps` in a Linux container on the M5 — so the peak is the exact
high-water mark (`VmHWM`) and the mapping table says **which mapping** holds it.

**The load**: `sumimi-gotest load` at 20 000/s, 20 s, 200 connections, loopback,
a fresh container for every point.

---

# 1. At the peak

| what | how much | how it was measured |
| --- | ---: | --- |
| the whole process | **212 MB** | `VmHWM` |
| **DuckDB's ledger** | **22.6 MB** | `sum(memory_usage_bytes) FROM duckdb_memory()` |
| … the in-memory table | 20.1 MB | the `IN_MEMORY_TABLE` tag |
| the queue | **1–3 MB** | `queued` × 1 000 B (`size_of::<Hit>()` is 1 000 B) |
| the in-flight bodies | **~0** | `inflightBytes` |
| the binary's code pages | 32.4 MB | the `/app/sumimi` mapping |
| **the rest** | **~150 MB** | anonymous mappings — and DuckDB does not account for it |

**The ledger is a sawtooth**: 2 → 13 → 22 MB → back to 2 MB. That is the WAL
filling up and the checkpoint writing it out. The WAL reaches 15.2 MB — DuckDB's
`checkpoint_threshold` is 16 MB — and the high-water mark jumps at exactly that
moment.

---

# 2. The decisive A/B

The same load, the same box; the only difference is the engine's memory limit
(or the backend).

| the engine's memory limit | the peak | the rate served | refused |
| --- | ---: | ---: | ---: |
| the default (244 MB on this box) | **215 MB** | 20 000/s | 0 |
| 128 MB | **178.8 MB** | 20 000/s | 0 |
| 96 MB | **148.6 MB** | 20 000/s | 85 247 (the gate follows the limit) |
| 64 MB | **108.0 MB** | 10 000/s | 190 220 (the gate) |
| 16 MB | **69.2 MB** | 2 500/s | 347 693 (the gate) |
| **the SQLite backend** | **32.8 MB** | **20 000/s** | 0 |

⇒ The whole request path — the queue, the writer, the parsing, the HTTP layer —
costs almost nothing: the SQLite backend does the same 20 000/s in 32.8 MB, code
pages included.

⇒ **The ~180 MB is DuckDB's ingestion**, and it is bounded by `memory_limit`:
**the peak ≈ the engine's limit + ~50 MB**.

---

# 3. The knobs that do **not** move it

| the knob | the peak | the note |
| --- | ---: | --- |
| the default | 215 MB | |
| `MALLOC_ARENA_MAX=1` | 210 MB | the allocator's arenas are not it |
| `MALLOC_TRIM_THRESHOLD_=65536` | 213 MB | the heap's trimming is not it |
| `MALLOC_MMAP_THRESHOLD_=16384` | 211 MB | |
| `SUMIMI_DUCKDB_THREADS=1` | 212 MB | |
| `preserve_insertion_order=false` | 213 MB | DuckDB's own advice for the ingestion memory does not apply here |
| `checkpoint_threshold=16MB`, **60 s** | 232 MB | |
| `checkpoint_threshold=32MB`, **60 s** | 239 MB | **it does not matter** |

⇒ **A 20-second run said 32 MB halved the peak (212 → 102 MB). That was the
measurement window, not the truth**: the checkpoint that spikes landed outside it.
Over 60 seconds the two are the same. ⇒ *measure long enough*.

---

# 4. What this means for a deployment

The tier's promise is about the whole process, so the engine's limit has to be
**the tier minus about 50 MB**:

- the minimum tier (a 128 MB engine, a 256 MB process) ⇒ **measures 178.8 MB**
  and still serves 20 000/s ⇒ the promise holds;
- the recommended tier (a 256 MB engine, a 512 MB process) ⇒ measures ~235 MB.

The default limit — a quarter of the machine's RAM, capped at 256 MB — is right
for the box it runs on; on a small box it is set explicitly.

---

# 5. The bug this found

`/api/heartbeat` reported DuckDB's memory as **one arbitrary tag**:
`duckdb_memory()` has one row per tag, and `SELECT memory_usage_bytes FROM
duckdb_memory()` read the first row. The heartbeat said 2.5 MB while the engine
held more. It is now `sum(memory_usage_bytes)` — which is what makes the
attribution above possible.
