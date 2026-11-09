# What the engine's limit costs — 2026-11-10

**The question**: the deployment rule says the engine's limit is the tier minus
60 MB. What does the limit actually buy, and what does it cost?

**The data**: one three-year history (6 227 088 page views + 1 026 291 events,
372 MB on disk), one report — the whole three years — and four engine limits.
Each limit gets its own server on the same directory, with the report cache
emptied before the report (the cache lives in the instance directory, so
without that the second server would answer from the first one's cache, which
is the wrong number).

**The machine**: Apple M5, 10 cores, 32 GB, macOS 25.5.0, sumimi 0.4.4.

---

# 1. The limit is a trade, not a test

| the engine's limit | the report | the peak memory | the spill on disk |
| --- | ---: | ---: | ---: |
| 512 MB | **133 ms** | 571 MB | 0 |
| 256 MB | 187 ms | 344 MB | 27 MB |
| 128 MB | 384 ms | 225 MB | 97 MB |
| 64 MB | 734 ms | **154 MB** | 140 MB |

**The answer is the same at every limit** — 6 216 328 page views, 1 965 056
visitors, 938 445 bounces, at 512 MB and at 64 MB, byte for byte. (Those are
the numbers from the by-hand run below, which asks the same window at every
limit; the tool measures the time, the peak and the spill.) DuckDB spills to
disk and answers anyway.

**What changes is the cost**: the report gets 5.5× slower from 512 MB to
64 MB, the process's peak falls from 571 MB to 154 MB (3.7×), and the disk
grows by 140 MB — the spill.

![The limit trades memory for time and disk](https://x-cmd.com/draw?h=520&labels=512MB%2C256MB%2C128MB%2C64MB&line=the+report+%28ms%29%3A133%2C187%2C384%2C734&series=the+peak+memory+%28MB%29%3A571.3%2C344.6%2C225.3%2C154.4&series=the+spill+on+disk+%28MB%29%3A0%2C27.3%2C97.8%2C140.6&title=The+limit+trades+memory+for+time+and+disk&type=combo&w=960)

---

# 2. Two corrections

**"The limit has to hold the report" is wrong.** The limit is the buffer
manager's ceiling; a query that needs more than that spills to disk and
answers. The limit decides how fast, and how big the process is — not whether
there is an answer.

**"A three-year summary does not fit a 256 MB box" is wrong.** It fits: at a
256 MB limit the three-year report answers in 187 ms, and the process peaks at
344 MB — 88 MB over the limit, and the spill is 27 MB of temporary disk.

---

# 3. The limit does not bound the process

The peak resident set minus the engine's limit:

| the engine's limit | the peak memory | the difference |
| --- | ---: | ---: |
| 512 MB | 571 MB | +59 MB |
| 256 MB | 344 MB | +88 MB |
| 128 MB | 225 MB | +97 MB |
| 64 MB | 154 MB | +90 MB |

So the process runs **60–97 MB over the engine's limit** — the code, the
allocator's arenas and the query's own vectors are not the buffer manager's
business. This is why the deployment rule is **engine limit = tier − 60 MB**
and not "engine limit = tier": the engine's own ceiling plus about 90 MB has
to fit the box.

---

# 4. The spill is temporary — and it is disk

The spill directory sits next to the database file (`data.duckdb.tmp`) and
**DuckDB deletes it when the query ends**. It is not growth; it is a peak.

But it is disk, and a peak disk is disk the box must have:

| the engine's limit | the spill, three years of history |
| --- | ---: |
| 256 MB | 27 MB |
| 128 MB | 97 MB |
| 64 MB | 140 MB |

The server's own ceiling on this is `max_temp_directory_size`, **2 GB by
default** (`SUMIMI_DUCKDB_TEMP_LIMIT`). On a 10 GB VPS that is a fifth of
the disk, in one query. The rule that follows: on a small box, set
`SUMIMI_DUCKDB_TEMP_LIMIT` to what the disk can spare — a spill that cannot
fit is a **failed report**, not a slow one.

---

# 5. How this was measured

**The tool**: `sumimi-gotest axes --limits 512MB,256MB,128MB,64MB` — the
public one, so the measurement does not inherit the server's bugs, and the
spill is watched while the report runs (the spill is temporary, so it cannot
be looked at afterwards).

**Cross-checked by hand**: the same four limits, measured with a shell script
that started the server, timed the HTTP call with `curl` and sampled the
spill directory every 20 ms:

| the engine's limit | the report (the tool) | the report (by hand) | the spill (the tool) | the spill (by hand) |
| --- | ---: | ---: | ---: | ---: |
| 512 MB | 133 ms | 166 ms | 0 | 0 |
| 256 MB | 187 ms | 235 ms | 27.3 MB | 26.8 MB |
| 128 MB | 384 ms | 478 ms | 97.8 MB | 97.4 MB |
| 64 MB | 734 ms | 897 ms | 140.6 MB | 141.9 MB |

The tool is a little faster (it does not copy the 390 MB database for every
point, and it times the HTTP call without `curl` in the way); the spill and
the peak agree to within a megabyte.

**The peak on macOS** is the kernel's `ru_maxrss` — the server cannot report
its own high-water mark on this platform. On Linux it reads its own
`VmHWM`, and that is the number the CI publishes.
