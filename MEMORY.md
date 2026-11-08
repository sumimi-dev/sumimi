# MEMORY.md — Memory management

> The strategy for **one thing**: keeping the whole process inside the memory box we
> promise, on a 512 MB VPS, under any traffic.
>
> AGENTS.md is the source of truth for the design; this file is the full statement of
> the memory half of it. The user-facing version of the same story is
> [`service/docs/docs/memory.html`](service/docs/docs/memory.html) (served at
> `/docs/docs/memory.html`, published at
> <https://sumimi-dev.github.io/docs/memory.html>).
>
> Open questions live in issues, not here: [#12](https://github.com/sumimi-dev/sumimi/issues/12)
> (the two big problems), [#13](https://github.com/sumimi-dev/sumimi/issues/13) (how DuckDB
> really manages memory), [#14](https://github.com/sumimi-dev/sumimi/issues/14) (what exactly
> decides the collector's peak).

---

## 1. The promise

| tier | the machine | the engine's ceiling | **the whole process** must stay under |
| --- | --- | ---: | ---: |
| **minimum** | 1 vCPU / 512 MB | 128 MB | **256 MB** |
| **recommended** | 1 vCPU / 1 GB | 256 MB | **512 MB** |

Two things are true about that table, and the second one is the one people forget:

1. **The number that matters is the process's peak RSS, not the engine's.** What a small
   box runs out of is *everything*: the code pages, the collector's strings, the queue, the
   pages the allocator keeps, and the engine's share.
2. **The engine gets a quarter of the machine, not the whole of it.** The other three
   quarters belong to the operating system, to the collector, to the queue and to the
   allocator — and until recently **nobody was budgeting for those at all**.

The promise is asserted, not hoped for: the daily CI job reads the exact high-water mark
(`/proc/self/status`, `VmHWM`) on Linux and fails the build when the process outgrows its
box.

## 2. The ledger: six accounts, and who bounds each one

| account | what it is | bounded by | measured |
| --- | --- | --- | --- |
| the binary and the stacks | the executable's text and data, the thread stacks | nothing (it is what it is) | 42 MB idle |
| **the engine's buffer pool** | DuckDB's own buffer manager | `memory_limit` — `min(RAM/4, 256 MB)` | 0.5–7 MB |
| **the engine's other allocations** | the appender's rows, the WAL, DuckDB's internal allocator | **nothing we set** | unknown — see [#13](https://github.com/sumimi-dev/sumimi/issues/13) |
| **the collector** | the HTTP collection path: the body, the JSON tree, the parsed hit | the admission gate (§5) | **the rate is the lever** — see §5 |
| **the queue** | the hits waiting to be written | `SUMIMI_MAX_QUEUE` = 100 000 **rows** | ~5 MB of structs per 10 000 rows, plus the strings |
| **the allocator's retention** | the pages glibc took and did not give back | **nothing** ⚠ | the peak never comes back down |

`duckdb_memory()` reports the **buffer manager only**. It is not the engine's total and it
is not the process's — reading it as either is how a 2 MB engine and a 636 MB process end
up in the same sentence.

## 3. The facts we have

| | peak RSS | the engine holds | the queue | where it was measured |
| --- | ---: | ---: | ---: | --- |
| idle | 42 MB | 0.5 MB | 0 | the daily budget run |
| the write path, 200 000 events | **168 MB** (128 MB engine) / **172 MB** (256 MB engine) | 7 MB | 0 | the daily budget run, Linux, exact peak |
| a burst: 1 GB injected in 60 s | **636 MB** | 2 MB | 0 at the peak | the soak (macOS, sampled) |
| the same burst, another day | **1560 MB** | 4 MB | 0 at the peak | the soak (macOS, sampled) |

Three things follow, and they are the whole story:

* **The engine is innocent.** 2–7 MB, in every run, at every traffic shape. DuckDB's own
  strategy works; the problem is that we never wrote a strategy around it.
* **The burst is the problem, and it is ours.** At the peak the queue is empty and the
  engine holds 2 MB — so ~600 MB is the collector's own allocations and the pages the
  allocator kept.
* **The same shape gave 636 MB and 1560 MB.** Variance like that means *one run is not a
  measurement* — every number in this file has to come from a median of at least three
  runs, on Linux, where the peak is exact rather than sampled.

## 4. The strategy: five lines, around the engine's own

DuckDB has a strategy of its own, and the rule is to **design around it, not against it**:

| DuckDB's rule | what it means for us |
| --- | --- |
| `memory_limit` bounds the **buffer pool**, not the data | the engine's share is a quarter of the machine, and we leave the rest alone |
| a query **streams and spills** to disk | a long query does not need the box's RAM, it needs the box's disk |
| the buffer is **flushed** back (`allocator_flush_threshold`, 32 MB) | DuckDB gives its own memory back; we have to give ours back too |
| **one thread per 64 MB** of budget | a parallel aggregate builds one hash table per thread, so the thread count *is* a memory rule |

Our five lines are the same shape — one per account in §2:

| | line | state |
| --- | --- | --- |
| **G1** | the engine's ceiling — `min(RAM/4, 256 MB)` | ✅ in place |
| **G2** | the write batch — `SUMIMI_MAX_BATCH` = 10 000 rows | ✅ in place, but counted in **rows**, not bytes |
| **G3** | **the admission gate before the queue** — nothing may be parsed without a bound | ✅ **built and on** (`service/src/limits.rs`) — the knobs are adjustable, and the rate's **default is derived from the engine's budget**: 20 000/s per 128 MB (40 000/s at 256 MB). `SUMIMI_MAX_ADMIT_RPS` overrides it |
| **G4** | the queue's capacity — `SUMIMI_MAX_QUEUE` = 100 000 rows | ⚠ in place, but 100 000 rows is ~50 MB of structs **plus the strings**, and the strings are the bigger half |
| **G5** | giving the allocator's pages back | ❌ missing (needs `malloc_trim`, which needs §5 rule 1 relaxed) |

### The invariants

These are the rules that make the promise true. A change that breaks one is a change that
breaks the product.

1. **Every allocation is bounded by something we set.** No path may allocate "as much as
   the traffic brings".
2. **A refusal must happen before the allocation it refuses.** Refusing after the work is
   done is not a limit; it is a delay.
3. **The engine gets a quarter of the machine; the rest of the process has its own
   budget**, and the two budgets are spent separately.
4. **The peak, not the average.** A budget is about the high-water mark: the moment the
   process is largest is the moment the box dies.
5. **Every number is measured on Linux**, where the peak is exact, and it comes from at
   least three runs.

## 5. The collection path: where the rate limit goes

This is the part that is missing (G3), and it is the part that produced the 636 MB.

### What happens today

```
POST /api/send
  └─ axum's Json extractor      reads the whole body into memory  (default limit: 2 MB)
     └─ serde_json::from_slice   a JSON tree, one allocation per string
        └─ into_hit()            flat = obj.clone()      ← a second copy
           └─ build_hit()        ~30 Option<String>     ← a third copy
              └─ queue.push()     the hit waits here
                 └─ writer        a batch of ≤10 000 rows is written to DuckDB
```

Three copies of the same strings are alive at once, and the queue can hold 100 000 hits.
**Nothing on this path has a limit of its own** — the only gate is the queue's row count,
and it is checked *after* the body has been read and the JSON has been parsed.

### The two problems

1. **There is no bound.** The collector accepts as much as the network brings, and the
   allocator's arenas grow to the high-water mark of whatever burst arrived.
2. **The refusal is too late.** The queue-full `503` is raised after the parse. The memory
   has already been spent by then, so refusing protects the *queue*, not the *box*.

### The target

```
POST /api/send
  └─ read the headers only         nothing is allocated yet
     └─ ADMIT                      ← the gate, before the body is read
        ├─ is the body within the per-request limit?
        ├─ does the in-flight byte budget have room for it?
        ├─ are we under the in-flight request limit?
        └─ is there a rate token?
     └─ read the body (bounded)     only now does a byte of it exist
        └─ parse (one copy)         stream it; do not build three copies
           └─ queue.push()          the queue is bounded in bytes
```

Four knobs, and each one is a different kind of limit:

| knob | environment variable | bounds | why it is not the others |
| --- | --- | --- | --- |
| **the per-request body limit** | `SUMIMI_MAX_BODY_BYTES` | one request | a single request must never be able to blow the budget |
| **the in-flight byte budget** | `SUMIMI_MAX_INFLIGHT_BYTES` | the collector's live working set | **bytes, not requests** — a request is not a memory unit |
| **the in-flight request limit** | `SUMIMI_MAX_INFLIGHT_REQUESTS` | the number of parsers | bounds the number of arenas the allocator touches |
| **the admission rate** | `SUMIMI_MAX_ADMIT_RPS` | requests per second | bounds the churn — the allocator's high-water mark grows with the *rate*, not with the total |

**Zero means "no limit"**, and the rate's default is **not** zero: a box that never
says no is a box that runs out of memory. When `SUMIMI_MAX_ADMIT_RPS` is **not set**,
the server uses **the recommended rate for its memory** ✓:

> **20 000 requests a second for every 128 MB of the engine's budget** ✓ — so the
> **minimum tier** (a 128 MB engine, a 256 MB process) admits **20 000/s** ✓ and the
> **recommended tier** (256 MB, 512 MB) admits **40 000/s** ✓.

It comes from the measured rule (§6 ✓): the peak is about **68 MB + 7.8 MB per 1 000
requests a second** ✓ — **and after the seven fixes (§6 ✓) that rule is no longer
the binding one** ✓ ⇒ ⇒ ⇒ the smallest box we measured runs out of **CPU** at
about **19 700 requests a second** ✓ ⇒ ⇒ ⇒ so 20 000/s is where that box is
**full but not over** ✓ ⇒ ⇒ ⇒ ⇒ **the memory is not what limits it any more** ✓
⇒ ⇒ ⇒ ⇒ **the limit is set where the box is full but not over** ✓ ⇒ ⇒ ⇒ ⇒ above
it the server refuses ✓ ⇒ ⇒ ⇒ ⇒ which is the honest answer ✓ ✓. The other three
knobs stay off by default ✓ — they are the backstop, not the lever ✓ ✓.

### The rate follows the writer

The fourth knob is not a constant, and this is the part that makes it "our own strategy"
rather than a guess:

* the writer knows how fast it drains the queue (rows per second, measured over the last
  few flushes);
* **the admission rate is set to the drain rate** (times a small safety factor) — never
  faster, because accepting hits we cannot write only fills the queue and ends in a
  refusal we could have avoided;
* when the queue is above a watermark, the admission rate drops and the answers carry a
  `Retry-After` computed from the drain rate — **backpressure instead of a wall**;
* the client (our tracker, and Umami's) already retries a `503`, so a refusal is a
  slowdown, not a loss. What we acknowledged, we have.

### The numbers are not decided yet

The *shape* of the gate is decided here; the *numbers* are not, because they depend on
which mechanism actually decides the peak — the rate, the in-flight volume, the live queue
or the engine's non-buffer-pool allocations. That is what
[#14](https://github.com/sumimi-dev/sumimi/issues/14) is measuring, one variable at a time.

The numbers will be derived, not guessed: **the in-flight byte budget is a share of the
tier's non-engine budget** (256 − 128 = 128 MB on the minimum tier), and the queue's
capacity in rows follows from the byte budget and the measured size of one queued hit.

### What the first sweep measured

The sweep (`pro/examples/gate_sweep.rs` — one variable at a time, three runs per
configuration, one process per run, the median) says something very plain:

| configuration | the peak RSS |
| --- | ---: |
| no gate (today) | **659 MB** |
| the body limit (64 KB) | 592 MB |
| the in-flight bytes (4 MB) | 594 MB |
| the in-flight requests (16) | 605 MB |
| **the rate (20 000/s)** | **188 MB** |
| **the rate (5 000/s)** | **95 MB** |
| all of them together | 189 MB |

(200 MB of payload over 20 s, 64 connections. Sampled on macOS — a lower bound; the CI
runs the same sweep on Linux, where the peak is exact.)

**The rate is the lever.** The other three knobs do nothing measurable on their own,
because the peak follows the **rate of allocation** — the allocator's churn — and not
the volume in flight. And the rate limit fixed a second thing for free: the queue never
filled once (0 dropped, against 6 233 without it).

So the strategy is: **limit the rate first**, and set the rate from what the writer can
actually drain — not from a constant. The in-flight limits stay as the backstop.

## 6. The deployment reference

**What the memory costs, by the rate.** Measured with the sweep
(`sumimi-gotest sweep` ✓ — the public tool ✓ ⇒ a second implementation ✓ ⇒ ⇒ it
does not inherit the server's bugs ✓) on a **2 vCPU Linux box** ✓ (AMD EPYC-Genoa
✓, 2 GB ✓) ⇒ ⇒ the peak is the server's own high-water mark ✓ (it reads
`/proc/self/status`'s `VmHWM` ✓), one page view per request (110 bytes ✓),
64 connections ✓, 20 s ✓, **a fresh server per point** ✓ ⇒ ⇒ ⇒ so every row is
the peak of *that* point ✓ ⇒ ⇒ ⇒ ⇒ and the promise is about the peak ✓ ⇒ ⇒ ⇒
so the peak is what the table carries ✓.

| the rate offered | the peak RSS | the engine holds | what it fits |
| --- | ---: | ---: | --- |
| 5 000 /s | **110 MB** ✓ | 2 MB | the **minimum** tier (256 MB) ✓ — 146 MB to spare ✓ |
| **10 000 /s** | **115 MB** ✓ | 2 MB | the **minimum** tier ✓ — 141 MB to spare ✓ |
| **20 000 /s** | **182 MB** ✓ | 3 MB | the **minimum** tier ✓ — **74 MB to spare** ✓ |
| 40 000 /s | **182 MB** ✓ | 3 MB | the **minimum** tier ✓ — and the box is at its ceiling ✓ |
| 100 000 /s | **~850 MB** (by the rule ✓) — measured **~600 MB** on a bigger box ✓ | | **neither** ✓ |
| 200 000 /s | **~600 MB, and it stops growing** ✓ | | the traffic is refused, not stored ✓ |

⇒ ⇒ **these numbers need `MALLOC_MMAP_THRESHOLD_=65536` in the environment**
✓ ⇒ ⇒ ⇒ without it the same box peaks at 138 / 145 / **208** / **210 MB** ✓ ⇒
⇒ ⇒ ⇒ ⇒ **−20% / −21% / −12% / −13%** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **that one line is worth
more than every table row** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ (the Docker image sets it ✓ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ a systemd deployment should ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ see the page ✓ ✓).

⇒ ⇒ **and it is not free** ✓ ⇒ ⇒ ⇒ the pages come back, but the CPU pays for
them ⇒ ⇒ ⇒ ⇒ **mmap/munmap are system calls** ✓ ⇒ ⇒ ⇒ ⇒ **−5% of the
throughput** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ (measured twice ✓ ⇒ ⇒ ⇒ ⇒ ⇒ 19 724 → 18 665/s ✓)
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **so it is a knob, not a free win** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ **the deployment picks it** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the Docker
image picks it for you** ✓ ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and on macOS it does
nothing at all** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **that is Apple's allocator,
not glibc's** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **so this table is a Linux
table** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and the CI workflow runs on a Linux
runner for the same reason** ✓ ✓.

⇒ ⇒ **and the rule changed** ✓ ⇒ ⇒ ⇒ it used to be **68 MB + 7.8 MB per 1 000
requests a second** ✓ ⇒ ⇒ ⇒ ⇒ **now the curve is flat** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the same
box ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the same tool ✓ ⇒ ⇒ ⇒ ⇒ ⇒ **5 000/s: 103 → 110 MB** ✓ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ **10 000/s: 143 → 115 MB** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **so the growth over that
range is 1 MB per 1 000** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **seven times flatter** ✓ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and it stops at the ceiling** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ **182 MB
whether you offer 20 000 or 40 000** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the box
is the limit now, not the memory** ✓ ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the
flat part is the seven fixes** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and the 7 MB
at the bottom is the batch waiting to be written** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ (the M5 says the same ✓ ⇒ ⇒ ⇒ 133 → 139 MB at 5 000/s ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **so it is a constant, not a slope** ✓ ✓)
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ✓.

**And the box's ceiling rose** ✓ ✓ (the same box ✓, the same tool ✓, back to
back ✓ ⇒ ⇒ ⇒ **two runs each** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ so it is not one lucky run ✓ ✓):

| the rate offered | v0.4.1 | **v0.4.2** ✓ | v0.4.2 + the allocator ✓ |
| --- | ---: | ---: | ---: |
| 5 000 /s | 136 / 138 MB | 138 / 138 MB | **110 / 111 MB** ✓ |
| 10 000 /s | 145 / 146 MB | 145 / 146 MB | **115 / 115 MB** ✓ |
| 20 000 /s | 221 / 234 MB (**16 751 / 16 872/s** ✓) | **208 / 208 MB** (**19 656 / 19 792/s** ✓) | **182 / 183 MB** (18 604 / 18 725/s ✓) |
| 40 000 /s | 221 / 239 MB (**16 931 / 16 935/s** ✓) | **210 / 210 MB** (**19 281 / 19 683/s** ✓) | **178 / 186 MB** (18 317 / 18 490/s ✓) |

⇒ ⇒ **the CPU-bound box now serves 17% more traffic** ✓ ✓ ⇒ ⇒ ⇒ (the batching
⇒ ⇒ ⇒ 1 000 transactions became 3 ✓ ⇒ ⇒ ⇒ ⇒ ⇒ the CPU was the ceiling ✓ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ **the ceiling moved** ✓ ✓) ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ and at the same rate the
peak is **−9%** ✓ ✓.

⇒ ⇒ **the same measurement on an Apple M5 says the same shape** ✓ ⇒ ⇒ ⇒ (the
peak follows the rate ✓ ⇒ ⇒ ⇒ ⇒ at the low rates v0.4.1 and v0.4.2 are the
same ✓ ⇒ ⇒ ⇒ ⇒ ⇒ at the top v0.4.2 is cheaper ✓ ⇒ ⇒ ⇒ ⇒ 318 → 270 MB ✓ ✓)
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **but macOS cannot repeat itself** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ (its peak is
the kernel's `ru_maxrss` ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ which counts the shared pages ✓
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ the same binary at 5 000/s measured 104 MB in one
round ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ and 133 MB in the next ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **Linux repeats to ±2 MB** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **so the table only believes Linux** ✓ ✓ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and the CI workflow runs on a Linux runner** ✓ ✓
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ (the reports: `benchmarks/sweep-epyc-2026-11-08-repeat.md` ✓
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ and `benchmarks/sweep-m5-2026-11-08-repeat.md` ✓ ✓).

**And the rule stops applying at the top** ✓: on a bigger box, 100 000, 150 000 and
200 000 requests a second all land at **~600 MB** ✓ ✓ — because the server parses
about **113 000 requests a second** and no more ✓, and everything above that is
**refused** ✓ (587 511 of them at 200 000/s ✓) ✓. So 200 000/s is not a "more
memory" regime, it is a **"most of it is turned away"** regime ✓ ✓ — a real 200 000/s
needs a stronger box and a faster write path ✓, not more RAM ✓ ✓.

**That is where the default limit comes from** ✓: the rate is set where the box is
**full but not over** ✓ ⇒ **20 000 a second per 128 MB of the engine's budget** ⇒
**20 000/s on the minimum tier ✓, 40 000/s on the recommended one ✓** ✓ ⇒ ⇒ ⇒ ⇒
**and now the smallest box we measured is full there too** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ (its CPU
runs out at about 19 700/s ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **so the memory is no longer what
limits the minimum tier** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **the CPU is** ✓ ✓) ⇒ ⇒ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **which is a different reason for the same number** ✓ ✓ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and it is the honest one** ✓ ✓ ✓

⇒ ⇒ **and the promise now holds with room to spare** ✓ ⇒ ⇒ ⇒ the minimum tier
at its own recommended rate ⇒ **182 MB of 256 MB** ✓ ⇒ ⇒ ⇒ ⇒ (before: 256 MB of
256 MB ✓ ⇒ ⇒ ⇒ ⇒ exactly the ceiling ✓ ⇒ ⇒ ⇒ ⇒ **now: 74 MB to spare** ✓ ✓) ⇒
⇒ ⇒ ⇒ ⇒ ⇒ **that was [#17](https://github.com/sumimi-dev/sumimi/issues/17)'s
complaint** ✓ ⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **and it is answered** ✓ ✓ ✓.


**The event size counts as much as the rate.** At 20 000 requests a second, changing
only the size of one event:

| the body | the payload | the peak | the queue |
| --- | ---: | ---: | --- |
| 110 bytes (a page view) | 2 MB/s | 275 MB | empty ✓ |
| 1 KB | 20 MB/s | 407 MB | empty ✓ |
| **10 KB** | **200 MB/s** | **1812 MB** ✗ | **overflowed** ✓ (115 200 refused ✓) |

⇒ **the write path's capacity is in bytes a second** ✓, not in rows ✓ — at 200 MB/s
the disk could not keep up ✓ and the server refused ✓ ✓.

**What does *not* matter**, measured one at a time: the number of connections (16, 64
and 256 → 250, 275 and 255 MB ✓) ✓ and the two in-flight limits ✓ (248 and 243 MB ✓)
— they are the **backstop** ✓, not the lever ✓ ✓.

**So the rule for a deployment is three lines:**

1. **How many requests a second does the site get, and how big are the events?** ✓
   → the two tables ✓ — **the payload bytes a second is the number that matters** ✓ ✓;
2. **Does it fit?** ✓ → the first table ✓ — **10 000 page views a second costs
   149 MB** ✓ ✓, which is the **minimum** tier ✓ with 107 MB to spare ✓ ✓;
3. **Set `SUMIMI_MAX_ADMIT_RPS` to that rate** ✓ — then the peak is the table's row
   whatever the traffic does ✓, and a burst above it is refused (with a
   `Retry-After` ✓) instead of taking the box with it ✓ ✓.

### The same numbers on very different machines ✓

**Two machines is an observation, not a law** ✓ ✓ — the doubt has its own issue ✓
([#15](https://github.com/sumimi-dev/sumimi/issues/15) ✓): two points cannot prove a
rule ✓, and these two might be a coincidence ✓ ✓. What the measurement does say is
that **at the same rate, the two machines cost the same memory** ✓ ✓.

The table above is from a **2 vCPU Xeon** ✓. The same sweep on an **Apple M5 (10
cores)** ✓ gives:

| the rate offered | the 2 vCPU Xeon | the 10-core M5 |
| --- | ---: | ---: |
| 5 000 /s | 116 MB | 117 MB |
| 10 000 /s | 148 MB | 148 MB |
| 20 000 /s | 253 MB | 271 MB |
| 30 000 /s | 309 MB | 302 MB |
| what the machine could serve | **~43 000 /s** | **~113 000 /s** |
| the peak at the ceiling | 451 MB | 601 MB |

⇒ **at the same rate, the memory is the same** ✓ ✓ — so **a stronger CPU does not
mean less memory for the same traffic** ✓ ✓; it means **the same memory reaches a
higher rate** ✓ ✓ (and at the ceiling the M5 uses *more* ✓ — it churns more ✓);
⇒ ⇒ **the deployment reference is two questions**: *how much traffic do you get?* ✓
(the memory table ✓) and *can this box take it?* ✓ (the CPU ✓ and the disk ✓) ✓ ✓.

**So the number to measure on the box you ship on is the rate, not the memory** ✓ —
the memory follows from it ✓ ✓ — **and now that a third machine says the same thing ✓,
the table is "measured on three boxes" ✓, not a law on two ✓** ✓.

### The third machine says the same thing ✓ ✓ ✓

The doubt had its own issue ([#15](https://github.com/sumimi-dev/sumimi/issues/15) ✓)
— *two points cannot prove a rule, and the two might be a coincidence* ✓. So: a
**third** machine, a different CPU, a small plain VPS — **AMD EPYC-Genoa, 2 vCPU,
2 GB** ✓ — measured with the **released v0.4.0 binary** ✓ and the Go tool ✓
([sumimi-gotest](https://github.com/sumimi-dev/sumimi-gotest) ✓), on Linux ✓, so the
peak is the exact high-water mark ✓.

| the rate offered | the EPYC (2 vCPU, 2 GB) | the Xeon (2 vCPU) | the M5 (10 cores) |
| --- | ---: | ---: | ---: |
| 5 000 /s | **103 MB** | 116 MB | 117 MB |
| 10 000 /s | **143 MB** | 148 MB | 148 MB |
| 15 000 /s | **202 MB** | — | — |
| 20 000 /s | 214 MB (the box served 16 676/s) | 253 MB | 271 MB |
| 30 000 /s | 215 MB (served 16 669/s) | 309 MB | 302 MB |
| 40 000 /s | 197 MB (served 17 097/s) | — | — |
| what the machine could serve | **~16 700 /s** | ~43 000 /s | ~113 000 /s |

⇒ **the same rule, the third time** ✓ ✓ ✓ — the peak ≈ 68 MB + 7.8 MB per 1 000
requests a second ✓: at 5 000 that is 107 MB (the EPYC: **103** ✓), at 10 000 it is
146 MB (**143** ✓), at 15 000 it is 185 MB (**202** ✓) ✓ ✓ ✓.

⇒ **so it was not a coincidence** ✓ ✓ ✓ — [#15](https://github.com/sumimi-dev/sumimi/issues/15)
is answered: at the same rate, **three** machines of three different CPUs cost the
same memory ✓ ✓; what the CPU decides is **how far up the rate axis you can go** ✓ ✓
(16 700/s here ✓, 43 000 on the Xeon ✓, 113 000 on the M5 ✓ ✓).

⇒ **and past the ceiling the memory stops growing** ✓ ✓ — at 20 000/s the EPYC's
peak is 214 MB and its queue is **0** ✓ ✓: the requests it cannot parse **never
enter** ✓ ✓ ⇒ **the memory is safe** ✓ ✓ — the price is paid in **time** ✓, not in
memory ✓ ✓.

⇒ **but the default rate was too high for it** ✗ ✗ — the EPYC's default is **40 000/s**
(the recommended tier ✓ ⇒ the engine's budget is min(RAM/4, 256 MB) = 256 MB ✓ ⇒
40 000/s ✓) while the box serves ~16 700/s ✓ ⇒ the requests above that are **not
refused** ✓ (the gate's limit is 40 000 ✓) ⇒ they **wait** ✓ ⇒ and the client waits
with them ✓ ⇒ **0.7% of them time out** ✗ ⇒ the client finds out late ✓ ⇒ this is
[#14](https://github.com/sumimi-dev/sumimi/issues/14) ✓ ✓.

### And the traffic does not have to be loopback ✓ ✓

Every number above was loopback traffic ✓. Now it is measured ✓ — and the first
measurement was **not controlled** ✓ (two different server processes on the EPYC ✓,
two different histories ✓) ⇒ it said "the memory is the same" ✓ ⇒ **and that was
wrong** ✗ ✗.

The controlled version ✓: **the same container** ✓, the same server ✓, the same rate
✓, **three runs each** ✓, the server **restarted** ✓ every time ✓ — once with the
load generator **inside** the container (loopback ✓) and once with it **on the
laptop**, over the **public internet** ✓ (39 ms of round trip ✓):

| the traffic | the three peaks | the worst | the promise (512 MB ⇒ 256 MB) |
| --- | --- | ---: | --- |
| loopback (the generator inside) | 215 / 207 / 206 MB | **215 MB** | ✓ holds |
| the public internet (the generator outside) | 261 / 251 / 244 MB | **261 MB** | ✗ **broken by 5 MB** |

⇒ **the network costs ~20% of the memory** ✓ ✓ — the requests arrive in batches ✓
(39 ms of waiting ✓) ⇒ the server parses a batch at a time ✓ ⇒ the peak is higher ✓.
⇒ **and the default rate was measured over loopback** ✓ ⇒ so it carries **no
margin** ✓ ⇒ **the minimum tier's recommended rate breaks the promise on a real
network** ✗ ✗ ⇒ [#17](https://github.com/sumimi-dev/sumimi/issues/17) ✓ ✓ (the
proposal: **15 000/s on the minimum tier, 30 000/s on the recommended one** ✓ ✓ —
by the network's numbers ✓, with ~20% to spare ✓ ✓).

⇒ **what the network costs is the tail too** ✓ ✓ — at 5 000/s over the internet,
**0.7% of the requests timed out** ✓ (529 of 75 000 ✓) against **0** over loopback ✓
⇒ **TLS is still unmeasured** ✓ — that one needs a certificate on the box ✓.

### The most deployment-like measurement ✓ ✓

A **container** is what a customer actually runs ✓ ⇒ so the numbers from inside one
(4 vCPU ✓, **the 128 MB engine tier** ✓ = the minimum tier ✓, the released v0.4.0 ✓)
are the ones to quote ✓:

| the rate offered | the peak | refused | the queue | the engine held |
| --- | ---: | ---: | ---: | ---: |
| 5 000 /s | 117 MB | 0 | 0 | 2 MB |
| 10 000 /s | 157 MB | 0 | 0 | 2 MB |
| 15 000 /s | 163 MB | 0 | 0 | 2 MB |
| **20 000 /s** (the tier's default ✓) | **208 MB** | 0 | 0 | 768 KB |
| 30 000 /s | 220 MB | **180 313** ✓ | 0 | 768 KB |

⇒ **the gate works** ✓ ✓ — the limit is 20 000/s ✓ ⇒ offering 30 000/s gets
**180 313 refusals** ✓ ⇒ the server admits 20 000/s ✓ ⇒ **the memory does not
grow** ✓ ✓ ⇒ **the box is protected** ✓ ✓.
⇒ and the EPYC, re-measured with the fixed tool ✓, lands on the same line ✓ ✓:
**100 / 143 / 207 / 236 MB** at 5 / 10 / 15 / 20 000/s ✓ ✓ — its ceiling is
**~16 700/s** ✓ (two slow cores ✓), and past it the memory **stops growing** ✓ ✓.

⇒ **and a container has one more trap** ✗ ✗ — the server reads `/proc/meminfo` ✓ ⇒
the **host's** memory ✓ ⇒ in a `--memory=512m` container it thinks it has 7.7 GB ✓ ⇒
it sizes the engine at 256 MB ✓ and the gate at 40 000/s ✓ ⇒ **the container gets
OOM-killed** ✗ ⇒ [#16](https://github.com/sumimi-dev/sumimi/issues/16) ✓ ✓ ⇒
**fixed in v0.4.1** ✓ ✓: the cgroup's limit is read too ✓ ⇒ the **smaller** of the
two wins ✓ ⇒ a 512 MB container gets the minimum tier ✓ ✓.

### Where the memory actually goes ✓ ✓ (measured, [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓)

The old rule — **7.8 MB per 1 000 requests a second** ✓ ⇒ ⇒ ⇒ the one §6 records
before the seven fixes ✓ ⇒ ⇒ ⇒ — was the **allocator's retention of the churn** ✓,
not the data ✓ (the body is 109 bytes ✓). The
**churn**, measured with a counting allocator ✓ (`service/tests/allocations.rs` ✓):

| the stage | bytes per request |
| --- | ---: |
| the body itself | 109 B |
| parsed into a `serde_json::Value` **tree** | 679 B |
| … and **cloned** into a map (`into_hit()` ✓) | 713 B |
| the user agent | 151 B |
| building the hit (the struct is 1 000 B ✓) | 2 326 B |
| the HTTP layer | 894 B |
| **the whole request** | **26 347 B** |

⇒ **26 KB per request**, while the table assumes 7.8 KB ✓ ⇒ ⇒ **there is room
to optimise** ✓ ⇒ and the biggest lever found so far is **the queue** ✓:

> **a push into a queue of 0 rows costs 2 631 B; into a queue of 1 000 rows it
> costs 10 331 B** ✓ ✓ — the queue is a `VecDeque` ✓ and **growing it moves
> every hit in it** ✓ ⇒ ⇒ **the queue's length is the link between the rate
> and the churn** ✓ ✓ ✓ ⇒ which is *why* the memory follows the rate ✓ ✓.

⇒ and the writer **writes as soon as the queue is not empty** ✓ ⇒ ⇒ **a request
is usually a batch of one** ✓ ⇒ ⇒ the "10 000 rows a batch" never happens ✓ ⇒
⇒ both the throughput ceiling ✓ and the churn ✓ ⇒ [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓ ✓.

### The seven fixes ✓ ✓ (v0.4.2 ✓, [#18](https://github.com/sumimi-dev/sumimi/issues/18) ✓)

Each one was measured with the same ruler ✓ (the counting allocator ✓, the
release build ✓) and each step is in the issue ✓ ⇒ ⇒ **the issue is the
record** ✓ ✓.

| # | what was wrong | what it cost | the fix |
| ---: | --- | ---: | --- |
| 1 | the queue was a `VecDeque` ⇒ **growing it moved every hit in it** | 10 331 B per push | a chain of blocks ⇒ **1 054 B** ✓ |
| 2 | the writer wrote **as soon as the queue was not empty** ⇒ a request was a batch of one | 1 000 transactions per 1 000 requests | the writer **waits for a batch** ✓ ⇒ 3 transactions ✓ |
| 3 | `into_hit()` **copied the whole tree** | 713 B | the map is **moved** ✓ ⇒ 0 B ✓ |
| 4 | the payload was a **`Value` tree** ⇒ every field copied out of it again | 679 B of tree | parsed **straight into the fields** ✓ ⇒ 49 B ✓ |
| 5 | the fields were **copied** into the event | a few hundred B | they are **moved** ✓ |
| 6 | the channel lookup **lowercased the referrer once per domain** ⇒ over a hundred times | **1 731 B** ✓✓ | the needles are lowercase constants ✓ ⇒ **nothing is allocated** ✓ |
| 7 | the allocator **kept the pages it took** | **23% of the peak** ✓ | `MALLOC_MMAP_THRESHOLD_=65536` ✓ |

⇒ **the request path, now** ✓ (the same ruler ✓):

| the stage | before | **now** |
| --- | ---: | ---: |
| the body, parsed | 697 B | **49 B** ✓ |
| building the hit (the tracker's own body ✓) | 3 493 B | **898 B** ✓ |
| the whole request | 26 347 B | **23 128 B** ✓ |
| a push into a queue of 1 000 rows | 10 331 B | **1 054 B** ✓ |

⇒ ⇒ **−12% of the request's memory** ✓ ✓ ⇒ and the peak follows at the high
rates ✓ ⇒ ⇒ ⇒ ⇒ **the answer to "does the memory follow the implementation?"
is yes** ✓ ✓ ✓.

⇒ **and the biggest single item is not the peak** ✓ ⇒ the **write path** costs
**9 966 B per row** ✓ ⇒ ⇒ **four vectors per row** ✓ ⇒ ⇒ three of them inside
the DuckDB crate ✓ ⇒ ⇒ ⇒ but they are **churn, not residency** ✓ ⇒ ⇒ ⇒ the
allocator hands the same pages back for the next row ✓ ⇒ ⇒ ⇒ ⇒ so it costs
**CPU**, not memory ✓ ⇒ ⇒ ⇒ [#19](https://github.com/sumimi-dev/sumimi/issues/19) ✓ ✓.

### How to repeat the measurement

Three rulers. The first one is the exact one.

| the ruler | the command | what it answers | how long |
|---|---|---|---|
| the allocations per request | `cargo test -p sumimi-service --test allocations -- --nocapture --test-threads=1` | how many bytes and how many allocations each request costs — deterministic, the same every run | seconds |
| the peak, end to end | `sumimi-gotest sweep --binary <the released sumimi-server> --rates 5000,10000,20000,40000 --seconds 20 --settle 5` | the peak resident set and the rate actually served — a fresh server per point, so every row is that point's own peak | two minutes |
| the peak, on a CI runner | `.github/workflows/measure.yml` — dispatched by hand | the same, for anybody else to reproduce: every release in one run, on one runner, so the versions are comparable | ten minutes |

The first one needs nothing but a checkout, and it is the one that says where the
memory goes. The second one needs a quiet machine and the released binary. The
third one needs the workflow on `main` — it is not there yet, it goes in with the
merge.

The second one's setup: the tool is the public one, and the password comes from
the environment, because that is where the server it starts gets it too:

```sh
export SUMIMI_ADMIN_PASSWORD=sumimi-gotest
MALLOC_MMAP_THRESHOLD_=65536 sumimi-gotest sweep --binary ./sumimi \
  --rates 5000,10000,20000,40000 --seconds 20 --settle 5 --out report.md
```

### A measurement trap worth writing down ✓ ✓

**`ru_maxrss` on Linux is not the child's peak** ✗ ✗ — Linux does **not** reset it on
`exec` ✓, so a child's high-water mark still carries the parent's resident set from
the moment of the fork ✓ ✓. The load generator is a big process by the third point ✓
⇒ its memory was reported as the server's peak ✓ ⇒ **the same rate measured 418 MB
and then 831 MB** ✗ ✗ — while the server's own `/proc` `VmHWM` stayed at 130~140 MB ✓
✓ ✓.

⇒ **the server reports its own peak** ✓ (`/api/heartbeat` ⇒ `peakRssBytes` ✓) and
**that is the number to believe** ✓ ✓; `ru_maxrss` is only the fallback for the
platforms with no `/proc` ✓ (macOS resets it on `exec` ✓, so there it is clean ✓).

**The mechanism, tested where it is cheapest to test** ✓: the same machine ✓, the same
rate ✓, only the **thread count** changed ✓ — glibc gives every thread its own arena
✓, so the thread count *is* the arena count ✓:

| threads | the peak (10 000 /s ✓) |
| ---: | ---: |
| 1 | 139 MB |
| 2 | 144 MB |
| 4 | 138 MB |
| 10 | 139 MB |

⇒ **one thread and ten threads cost the same** ✓ ✓ — the arena count does not matter ✓
⇒ ⇒ the memory follows the **churn** ✓, the churn follows the **rate** ✓, and the
machine's capability enters only through **how high the rate can go** ✓ ✓ ✓.

## 7. Where each piece is decided

| question | where |
| --- | --- |
| the promise, the ledger, the invariants | this file, and [AGENTS.md §4.1a](AGENTS.md) |
| **the deployment reference — what it costs, by the rate** | **§6 of this file** ✓ |
| **how the numbers are measured** | **[sumimi-gotest](https://github.com/sumimi-dev/sumimi-gotest)** — the load generator and the probe, in Go. The server reports its own machine and its own peak in `/api/heartbeat`, so the report says which machine was measured |
| how DuckDB manages its own memory | [#13](https://github.com/sumimi-dev/sumimi/issues/13) |
| what decides the collector's peak | [#14](https://github.com/sumimi-dev/sumimi/issues/14) |
| the two big problems, and the disk half | [#12](https://github.com/sumimi-dev/sumimi/issues/12) |
| the memory budget in the private edition | [sumimi-pro#5](https://github.com/sumimi-dev/sumimi-pro/issues/5) |
| the disk half of the private edition | [sumimi-pro#8](https://github.com/sumimi-dev/sumimi-pro/issues/8) |
