# The two engines, four axes — 2026-11-09

**The question**: DuckDB takes more memory than SQLite — and what did we record
as SQLite's disadvantages?

**The data**: the same three-year history (6 227 088 page views + 1 026 291
events) seeded into both backends on the same machine; the same load for the
ingestion; the reports timed on the first call for each range (a cache miss,
the page cache warm for both).

---

# 1. The four axes

| | **DuckDB** | **SQLite** |
| --- | ---: | ---: |
| the peak memory under load (20 000/s) | 214 MB | **33 MB** |
| the data on disk (three years) | **372 MB** | 3 086 MB |
| the summary of the last year | **145 ms** | 17 452 ms |
| the summary of the last three years | **186 ms** | 24 162 ms |
| the memory a 3-year summary needs | 322 MB | **54 MB** |

⇒ **DuckDB wins the disk (8×) and the latency (95–238×) and loses the memory
(6.6×).** The trade is not "DuckDB is better": it is the disk and the latency
against the memory — and the memory is the one the deployment promise is about.

**What we had recorded before** — SQLite's disadvantages are the **disk space**
and the **response time** — is right. What was missing is the third axis, the
memory, and that is the one DuckDB loses. It was missing because the heartbeat
read one arbitrary tag of `duckdb_memory()`; with that fixed, the axes are
measurable.

---

# 2. The latency needs a long range

A latency measurement on a freshly loaded database measures nothing. On the few
seconds of data a load test leaves behind, both backends answer in single-digit
milliseconds:

| | DuckDB | SQLite |
| --- | ---: | ---: |
| the first report after a 400 000-event load | 7.5 ms | 195.3 ms |

True, and worth nothing. Over a long range:

| the range | DuckDB `/stats` | SQLite `/stats` | DuckDB `/stats/pages` | SQLite `/stats/pages` |
| --- | ---: | ---: | ---: | ---: |
| 6 months | **96.8 ms** | 9 182.8 ms | **5.0 ms** | 1 042.2 ms |
| 1 year | **145.4 ms** | 17 451.9 ms | **7.4 ms** | 1 881.4 ms |
| 3 years | **185.7 ms** | 24 161.9 ms | **15.1 ms** | 3 599.3 ms |

⇒ **95–238× faster** ⇒ ⇒ and the difference only shows on the **first** report
after a write: the report cache answers the rest in under a millisecond on both.

---

# 3. The memory a report needs grows with the range

The process's resident set while the report runs, the same three-year dataset:

| the range | DuckDB | SQLite |
| --- | ---: | ---: |
| 6 months | 73.0 MB | 54.0 MB |
| 1 year | **217.7 MB** | 54.1 MB |
| 3 years | **322.0 MB** | 54.1 MB |

⇒ DuckDB's report memory grows with the range (the hash tables of the
aggregate); SQLite's stays flat.

---

# 4. The engine's limit does not decide whether the report works

**It decides how fast it is, and how big the process gets.** The same
three-year dataset, the same reports, the limit and the thread count changed:

| the engine's limit | 6 months | 1 year | 3 years | the peak RSS (3 years) |
| ---: | ---: | ---: | ---: | ---: |
| 64 MB | 318.8 ms | 549.9 ms | 691.2 ms | 154.1 MB |
| 96 MB | 293.1 ms | 517.8 ms | 693.2 ms | 183.9 MB |
| 128 MB | 178.0 ms | 278.8 ms | 371.8 ms | 239.9 MB |
| 160 MB | 182.8 ms | 295.7 ms | 388.8 ms | 283.6 MB |
| 256 MB | **97.1 ms** | **144.6 ms** | **180.4 ms** | 319.1 MB |

⇒ **every limit answers** ⇒ ⇒ DuckDB **spills to disk** rather than failing ⇒ ⇒
⇒ **a query that does not fit is slow, not fatal** ✓ ⇒ ⇒ ⇒ the limit buys
**3.8× of the speed** (691 ms → 180 ms) ✓ ⇒ ⇒ ⇒ and the process's peak is
**the limit plus about 60 MB** ✓ ✓.

**The threads** (at a 128 MB limit):

| the threads | 6 months | 1 year | 3 years | the peak RSS (3 years) |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 287.6 ms | 481.9 ms | 691.8 ms | 229.9 MB |
| 2 | 178.8 ms | 284.0 ms | 374.2 ms | 240.1 MB |
| 4 | **98.7 ms** | **149.8 ms** | **210.0 ms** | 229.9 MB |

⇒ **more threads are faster and cost no more memory here** ⇒ ⇒ ⇒ the peak is
the same at one thread and at four ⇒ ⇒ ⇒ ⇒ so for *this* query the memory is
**not** "one hash table per thread" ⇒ ⇒ ⇒ ⇒ ⇒ (the rule in `config.rs` is
about a parallel `COUNT(DISTINCT …)`; the summary is not that) ✓.

---

# 5. What this means for the tiers

| tier | the engine's limit | the 3-year summary | the process's peak | fits? |
| --- | ---: | ---: | ---: | --- |
| the minimum (256 MB process) | 128 MB | 371.8 ms | **239.9 MB** | **yes — 16 MB to spare** ✓ |
| the recommended (512 MB process) | 256 MB | 180.4 ms | 319.1 MB | yes, comfortably ✓ |

⇒ **the minimum tier answers a three-year summary, with 16 MB to spare** ✓ ⇒ ⇒
⇒ that is the measurement behind the tier table ✓ ⇒ ⇒ ⇒ ⇒ **and the earlier
worry — "a three-year summary does not fit a 256 MB box" — is wrong** ✓ ⇒ ⇒ ⇒ ⇒
⇒ ⇒ ⇒ **it fits; it is the *engine's limit* that has to be low enough** ✓ ⇒ ⇒
⇒ ⇒ ⇒ ⇒ ⇒ ⇒ **⇒ ⇒ ⇒ the rule is: the engine's limit = the tier − 60 MB**

---

# 6. Two lessons worth keeping

**A latency measurement on a freshly loaded database measures nothing.** On the
few seconds of data a load test leaves behind, both backends answer in
single-digit milliseconds. The difference only appears over a long range — and
that is the range a user actually asks for.

**The engine's limit does not decide whether a report works.** DuckDB spills to
disk: at a 64 MB limit the three-year summary still answers, in 691 ms instead
of 180 ms. What the limit decides is the *speed* and the *process's peak* — and
the peak is the limit plus about 60 MB. So the sizing rule is
**the engine's limit = the tier − 60 MB**, not "the limit has to hold the
report".

