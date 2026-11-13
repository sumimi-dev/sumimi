# AGENTS.md — Sumimi (数咪咪)

> This file is the **single source of truth for the design** of Sumimi.
> Read it before writing code. If code and this document disagree, this document wins
> until it is updated in the same change.

---

## 0. TL;DR

Sumimi is a **self-hosted, single-binary web analytics service**: Rust + Axum + Tokio in the
backend, **DuckDB 2.0.0** as the embedded analytics store, and an **Umami v2–compatible HTTP
API** so existing Umami dashboards / trackers keep working.

The deployment target is a **cheap VPS**, not a cloud platform. There are two tiers,
and **every test runs against both of them**:

| tier | the machine | the engine's ceiling | the **whole process** must stay under |
| --- | --- | ---: | ---: |
| **minimum** | 1 vCPU / 512 MB (a 0.5G VPS) | 128 MB (a quarter of the machine) | **256 MB** |
| **recommended** | 1 vCPU / 1 GB (a normal VPS) | 256 MB (a quarter of the machine) | **512 MB** |

No Cloudflare, no Kubernetes, no external database, no object storage required.
One binary + one DuckDB file = the whole system.

The number that matters is the **process's** resident set, not the engine's: what a
small box runs out of is everything — the code pages, the writer's queue, the pages
the allocator keeps, and the engine's share. So the budget is asserted against the
peak RSS (`/proc/self/status`, `VmHWM`) on Linux CI, not against the engine's own
number. The engine's number is a part of it.

Measured footprint (release binary, DuckDB 2.0.0 statically linked):

| | idle | after 2 000 events + 200 queries |
| --- | ---: | ---: |
| process RSS | 42 MB | 113 MB (allocator retention, see below) |
| DuckDB buffer manager | 0.5 MB | 1.8 MB |
| DuckDB memory limit (declared) | 122 MiB (`SET memory_limit='128MB'`) | — |
| DuckDB threads | = core count (1 on a 1-vCPU box) | — |
| database file | 3.5 MB for 4 000 sessions | — |

The declared limit is a **ceiling, not an allocation**: DuckDB's buffer manager held
0.5–1.8 MB while serving traffic. The RSS growth after a burst is the C allocator
keeping freed pages, not live data. `SUMIMI_DUCKDB_MEMORY_LIMIT` tunes the ceiling;
`/api/heartbeat` reports the usage, the configured limit, the limit DuckDB itself
reports back, and the thread count.

A budget run — 200 000 realistic page views through the real write path
(`cargo run --release --example memory_budget -- --engine 128MB`):

| | 128 MB engine | 256 MB engine |
| --- | ---: | ---: |
| threads | 2 | 4 |
| write latency | p50 17 ms, p99 97 ms | p50 17 ms, p99 98 ms |
| the engine holds | 6 MB | 6 MB |
| the data file | 8.0 MB | 8.0 MB |
| dropped | 0 | 0 |

### 4.1a Designed for a 512 MB VPS

The defaults are chosen for the deployment target, not for a workstation:

| Setting | Default | Why |
| --- | --- | --- |
| `SUMIMI_DUCKDB_MEMORY_LIMIT` | **min(256 MB, RAM / 4)** | DuckDB's own default is 80% of RAM — 410 MB on a 512 MB box, which would leave nothing for the OS. The limit caps the *buffer*, not the data: queries stream and spill to disk. A 512 MB box gets 128 MB, a 1 GB box gets 256 MB, and nothing gets more than 256 MB — the engine streams long before that, and the cap is what keeps a big machine's engine from eating a small box's page cache. |
| `SUMIMI_DUCKDB_FLUSH_THRESHOLD` | 32 MB | DuckDB's default (128 MB) keeps that much freed memory before returning it to the OS. |
| `SUMIMI_DUCKDB_TEMP_LIMIT` | 2 GB | A query that spills must not fill the disk. |
| `SUMIMI_DUCKDB_THREADS` | auto (one per 64 MB of budget) | A parallel `COUNT(DISTINCT …)` builds **one hash table per thread**, so a query's memory scales with the thread count: at 10 threads a 5M-row summary query runs out of a 128 MB limit, at 2 threads it takes 154 ms. |
| `SUMIMI_MAX_BATCH` | **10 000** | The most events written in one batch. **The batch is what the write path costs in memory** — a batch is one transaction, and its rows sit in memory while it is written. The flush interval says how *often* we write; this says how *much*. Without it, two seconds of traffic at 170 000 views a second is 340 000 rows — 170 MB in one transaction, and the measured peak RSS was 1.5 GB. With it, the write path's memory does not depend on the rate. |
| `SUMIMI_STORE_TITLES` | **on** | On by default: Umami stores the title, and that is the baseline. Set it to `0` to save the space — the title is roughly half of the stored bytes and almost nobody looks at it. |
| `SUMIMI_STORE_FULL_REFERRER` | **on** | On by default. Set it to `0` to keep the referrer host alone — which is what the breakdown groups by anyway. |
| `SUMIMI_KEEP_QUERY` | **on** | On by default: Umami stores the query string, and the marketing parameters live in it (they are parsed out of it either way). Set it to `0` to drop the raw query string. |

Measured on 10M and 100M row tables (`cli/examples/scale.rs`, event-row schema,
128 MB DuckDB limit, the production thread derivation — 2 threads on a 128 MB budget):

| | 10M rows | 20M rows, realistic cardinality |
| --- | ---: | ---: |
| summary (30-day window) | 98 ms | 434 ms |
| timeseries | 12 ms | 40 ms |
| pages (top 50) | 5 ms | 158 ms |
| referrers | 5 ms | 18 ms |
| browsers | 25 ms | 105 ms |
| **sessions (50 rows)** | **675 ms** | **2 957 ms** |

All of it inside a **128 MB** DuckDB limit. The limit bounds RAM, not history:
DuckDB reads what it needs, aggregates it and spills the rest.

> **The session log is the slow one, and the summary breaks at scale.** The session
> log is the only panel that has to *derive* sessions from the event table — a hash
> aggregate over every event in the window, which spills once the window holds more
> sessions than fit in the budget. At 20M rows it is ~3 s; at 100M rows it is ~24 s.
>
> **Fixed:** the session log is a *log*, so it now reads the most recent N events
> (`ORDER BY created_at DESC LIMIT …`) and derives the sessions from those, instead of
> aggregating the whole window and throwing away all but 50. The rules live in one
> place (`storage::group_sessions`), so the three engines cannot disagree.
>
> At **100M rows the summary query runs out of memory** and the dashboard's first
> panel fails. The memory goes into the `COUNT(DISTINCT …)` hash tables — the session
> and visitor ids are 36-character UUID strings, and there are millions of distinct
> ones in a 30-day window.
>
> **Fixed:** the ids are DuckDB's 16-byte `UUID` type now (schema v4). The database is
> ~10% smaller and the hash tables hold a third of the bytes.

The thread count matters as much as the limit — see `SUMIMI_DUCKDB_THREADS` above.
Measured on the same 5M-row table, the summary query:

| threads | time | note |
| ---: | ---: | --- |
| 1 | 291 ms | what a 1-vCPU box gets |
| 2 | 154 ms | the default for a 128 MB budget |
| 4 | 80 ms | |
| 10 | 1 713 ms | and it can run out of the 128 MB limit |

The declared limit is a **ceiling, not an allocation**: DuckDB's buffer manager held
0.5–1.8 MB while serving traffic. The RSS growth after a burst is the C allocator
keeping freed pages, not live data. `SUMIMI_DUCKDB_MEMORY_LIMIT` tunes the ceiling;
`/api/heartbeat` reports the usage, the configured limit, the limit DuckDB itself
reports back, and the thread count.

**The memory strategy is [`MEMORY.md`](MEMORY.md)** — the promise, the ledger of where
the memory goes, the five lines and the invariants. Read it before touching anything
that allocates on the collection path. Two things from it belong here:

* **The engine is not the problem.** It holds 2–7 MB in every run, at every traffic
  shape. The memory is in **our** collection path and in the allocator's retention.
* **The admission gate** (`service/src/limits.rs`) sits **before the body is read**:
  `SUMIMI_MAX_BODY_BYTES`, `SUMIMI_MAX_INFLIGHT_REQUESTS`, `SUMIMI_MAX_INFLIGHT_BYTES`,
  `SUMIMI_MAX_ADMIT_RPS`. **Zero means "no limit"**. The rate is the one that is **on
  by default** — when it is not set, the server uses **the recommended rate for the
  memory it has** (20 000 requests a second per 128 MB of the engine's budget: 20 000/s
  on the minimum tier, 40 000/s on the recommended one), which is where the measured
  rule puts the box: full, but not over.

Measured, one variable at a time (200 MB of payload over 20 s, the median of three runs
per configuration — the sweep is `gate_sweep` in the private edition):

| configuration | the peak RSS |
| --- | ---: |
| no gate | **659 MB** |
| the body limit, the in-flight limits | 592–605 MB (no difference) |
| **the rate, 20 000/s** | **188 MB** |
| **the rate, 5 000/s** | **95 MB** |

**The rate is the lever** — the peak follows the *rate of allocation*, not the volume in
flight — and the rate limit fixed the queue overflow for free (0 dropped, against 6 233).
The rate that belongs in production is the writer's drain rate, not a constant.

**The deployment reference is [`MEMORY.md` §6](MEMORY.md)** — the peak by the rate
offered, the peak by the size of an event, and the three-line rule for sizing a box.
The short version: **10 000 requests a second costs about 150 MB** (2 MB of it the
engine's), which fits the minimum tier with room to spare.

```
                    ┌────────────────────────────────────┐
   browser ───────▶ │  sumimi (one binary)                │
   (tracker.js)    │  ├─ /sumimi.js   tracker             │
                    │  ├─ /api/send    collect             │
                    │  ├─ /api/*       umami compat + the │
                    │  │               control plane       │
                    │  ├─ /            app shell ─────────┼──▶ CDN (frontend JS)
                    │  ├─ /docs/*      docs site           │
                    │  └─ ./sumimi/    (the instance)      │
                    │      ├── data.duckdb     the events  │
                    │      └── instance.sqlite the control │
                    │                         plane        │
                    └────────────────────────────────────┘
```

**An instance is a directory.** The events (DuckDB or SQLite) and the control plane
(the accounts, the log, the imports, the cache, the saved reports, the chat — SQLite)
are two files side by side, so the accounts survive an analytics rebuild and the whole
instance is one path to back up, move or delete.

**One release artifact:**

| Artifact | What it is                                                                                   |
| -------- | -------------------------------------------------------------------------------------------- |
| `sumimi` | **everything**: `sumimi serve …` is the server, `sumimi summary …` is the client              |

The server and the client are **the same program**: they share the DuckDB schema, the
metric definitions and the query implementation (`service/src/reports.rs`), so they cannot
disagree — and there is only **one copy of DuckDB** in the binary. Installing the binary
under another name (`sumimi-server`, `sumimi-client`) switches the default mode; those are
symlinks in the release, not separate builds.

Brand: **Sumimi / 数咪咪** — "Sum" (count) + "咪咪" (kitty): counting the cats that
visit your sites. The engine is a tsunami — 10M views/day on one small box.
Slogan: *Count kitties popping into my websites. — 由猫开发*.

---

## 1. Repository map

This repository (`sumimi-dev/sumimi`, **public**) is a workspace of five components:

| Path       | Language       | What it is                                                                 |
| ---------- | -------------- | -------------------------------------------------------------------------- |
| `service/` | Rust           | The server library: collection API, stats API, auth, the writer, TLS, **the shared report layer**, **the instance store**. Also holds `service/docs/` — the website & documentation, embedded in the binary. |
| `cli/`     | Rust           | The analysis client. |
| `sumimi/`  | Rust           | The facade crate: the server and the client as one library, **and the one executable** (`sumimi`: server + client). |
| `tracker/` | TypeScript     | The tracker package — `@sumimi/tracker` on npm, `@ljh/sumimi` on JSR. One source (`src/mod.ts`), two registries. |
| `js/`      | JavaScript     | The tracker (`sumimi.js`) + the dashboard frontend (`sumimi-app.js`). Published on a CDN. |

The Rust crates are members of one Cargo workspace (root `Cargo.toml`). `service/` and
`cli/` share the schema and the report code by **code sharing**; the `sumimi/` crate is
the facade both of them are published through.

**The private edition depends on this one.** `sumimi-dev/sumimi-pro` does not copy these
crates — it depends on them (a git dependency, patched to the sibling checkout
locally) and adds the PostgreSQL backend, the object-storage archive and the jobs. So
there is exactly one implementation of the schema, the metrics and the reports.

### Two-repository split (important)

| Repository            | Visibility | Contents                                                                                        |
| --------------------- | ---------- | ----------------------------------------------------------------------------------------------- |
| `sumimi-dev/sumimi`   | public     | This repo: code, website, docs, public design (this file). **No private material, ever.**        |
| `sumimi-dev/sumimi-gotest` | public | The measuring instrument: the load generator and the probe. Go, the standard library only.     |
| `sumimi-dev/mneme`    | private    | Decision log, agent notes, private discussion issues, unreleased plans, draft design.            |
| `sumimi-dev/sumimi-dev` | private  | Org-level meta / non-code assets.                                                                 |

**Rule:** anything that reveals private discussions, unreleased plans, private traffic data,
internal credentials, or personal notes belongs in `mneme` only. The public repo may *reference*
`mneme` by URL (e.g. "see the private decision log"), but must not reproduce its content.

---

## 2. Privacy & data protection (READ THIS BEFORE TOUCHING DATA CODE)

Analytics software handles data that identifies people. These rules are not stylistic —
they are **acceptance criteria**. A change that violates them does not merge.

### 2.1 The data we collect

**The baseline is Umami's field set.** We do not store less than Umami does — a feature
Umami has and we lack is a feature we owe our users. The list below is what the session
and event rows carry.

| Group | Fields |
| --- | --- |
| Page | `url`, `url_query`, `title` |
| Referrer | `referrer` (host), `referrer_path`, `referrer_query`, `referrer_name` (the display name) |
| Site | `hostname` |
| Environment | `browser`, `browser_version`, `os`, `os_version`, `device`, `screen`, `language`, `country`, `region`, `city`, `latitude`, `longitude` |
| Identity | `visitor_id` (the visitor), `visit_id` (the session), `user_id` (the visitor as the site owner knows them), `session_start` (when the session began) |
| Marketing | `utm_source`, `utm_medium`, `utm_campaign`, `utm_content`, `utm_term` |
| Ad click ids | `gclid`, `fbclid`, `msclkid`, `ttclid`, `lifatid`, `twclid` |
| Performance | `lcp`, `inp`, `cls`, `fcp`, `ttfb` (Core Web Vitals) |
| Events | `event_name`, `event_type`, `tag`, `data` |
| Revenue | `revenue` per session/event, with `currency` |
| Visitors | a sixteen-byte hash of the address, the user agent and the website — a visitor survives across days |

**What we never collect** — the same short list Umami keeps:

* cookies, `localStorage` identifiers, fingerprinting (canvas, audio, fonts, hardware)
* form contents, keystrokes, page text
* the visitor's address **in the logs** — there is no per-request log at all, and the
  administrator's log records no addresses

**What we do collect, and why** — the **readings**, not the raw strings: the country, the
region, the city, the browser, the system, the device. The **raw IP address and the raw user
agent are not stored by default** (`SUMIMI_STORE_RAW=1` turns them on): the visitor
identifier is already a hash of the two, so nothing is lost by not keeping the originals —
and the default is the clean one, with the switch in the site owner's hands (D-041).

**The visitor identifier's input is the whole IPv4 address, or the /64 of an IPv6 one.**
A /24 would merge everyone behind one office NAT into a single visitor; IPv6 keeps its /64
because the privacy extensions rotate everything after it.

**The geolocation database is the site owner's business.** The server ships no data file and
makes no outbound request; the owner downloads one with `sumimi geoip download` and it is
read from the instance directory at startup. Until then, the country comes from an edge
header — and nothing is looked up.

### 2.1a The customer is a site owner

Our user is the person who **runs the site**, not the person who visits it. They care
about two things: **making money** and **what it costs**. Every design decision is
measured against those two questions:

* does this help them earn more (where did the money come from — which post, which
  referrer, which campaign, which page)?
* what does it cost them (disk, memory, CPU, operations)?

Privacy is not the selling point. *"You can see where the money came from"* is.

### 2.2 Rules that apply to every component

1. **Cookieless by default.** The tracker sets no cookies and no persistent storage
   identifiers. Honour `navigator.doNotTrack` and Global Privacy Control
   (`navigator.globalPrivacyControl`) by not sending anything.
2. **No raw IPs in logs.** Access logs record method, path, status, duration. Query strings are
   stripped from logged paths (they routinely carry session tokens and PII).
3. **No request bodies in logs.** Payloads are validated and discarded, never logged verbatim.
4. **No outbound network calls.** The service never phones home. No analytics, no crash
   reporting, no update checks.
5. **Synthetic fixtures only.** Tests, seed scripts, docs examples and screenshots use invented
   domains (`example.com`, `sumimi.dev`) and invented data. Never paste real traffic into this
   repo.
6. **Retention is a feature, not an afterthought.** Events/sessions older than the configured
   retention window are prunable (`sumimi prune`), and the CLI must expose the same window.
7. **Secrets never enter the repo.** JWT secret, admin password, DB path with credentials come
   from the environment or a file with `0600`. `.env` is git-ignored; `*.duckdb` and `*.duckdb.wal`
   are git-ignored.
7a. **Never trade away a feature for storage.** The engine's job is to make the full
   Umami field set cheap to keep. If a schema change would drop a field we do not have
   an equivalent for, fix the schema — not the field list.
8. **Access control is default-deny.** Statistics and website management require a valid token
   except for the deliberately public endpoints (`/api/send`, `/api/collect`, `/sumimi.js`,
   `/api/heartbeat`).
9. **Vulnerability reports are private.** Ask the maintainer privately; do not open a public
   issue containing a working exploit or a real leaked value.

### 2.3 Privacy features the UI/API must expose

* `GET /api/websites/:id` returns the website's data-retention settings.
* Custom event properties are user-supplied free-form data: the documentation must warn that
  **sending PII in custom properties defeats the whole purpose**, and the tracker must not
  auto-attach anything sensitive.
* A `sumimi erase` CLI command wipes one website (or everything) from the database.

---

## 3. Brand, naming and IP guardrails

* The product is **Sumimi**. It is an independent open-source project.
* Compatibility is described **factually**: *"An open-source, high-performance alternative to
  Umami. Implements the Umami v2 HTTP API."* Never claim endorsement, affiliation or
  sponsorship, and never use Umami's name in a way that suggests it.
* **No Umami assets**: logo, artwork, UI screenshots, CSS, code or copy are copied. The UI
  (cats, paws, our own icons) is original work.
* Third-party names (Umami, DuckDB, Axum, Cloudflare) are referenced nominatively as
  compatible/comparable products, per the nominative-fair-use principle.
* **Marketing assets must be original or properly licensed.** Do not rip frames from
  copyrighted anime/film for promo videos; a self-drawn or clearly transformative parody is
  safe, a clip montage is not. Any third-party artwork in `docs/` needs its licence recorded
  in `docs/assets/CREDITS.md`.
* The licence is **Apache-2.0** (see `LICENSE` and `NOTICE`). It was MIT until
  2026-11-04; the MIT grant for the versions published before that stands, but
  everything since is Apache-2.0. The reasons: an explicit **patent grant** for the
  contributors, and a licence the legal department of a company already knows.
  Dependency licences must stay permissive (DuckDB: MIT, Axum/Tokio: MIT/Apache-2.0).

---

## 4. Architecture

### 4.1 service/ — the server

* **Runtime:** Rust 2021, Axum 0.8, Tokio (multi-thread runtime).
* **Storage:** DuckDB 2.0.0, embedded, one file. `duckdb` crate `1.20000.x`
  (the crate encodes the DuckDB version as `1.MAJOR_MINOR_PATCH.x`, so `20000` = **2.0.0**).
  DuckDB runs with an explicit `memory_limit` (default 256 MB) so a 512 MB VPS survives.
* **Static, single-file build:** DuckDB is compiled into the executable (`bundled`
  feature, the default). The release is **one self-contained binary** — copy it, run it,
  done: no shared library to ship, no `LD_LIBRARY_PATH`, no rpath, no runtime surprises.
  This is the deployment that matters: *"one file on a cheap VPS"*.
  Building with `--no-default-features` produces a much smaller executable that links
  the prebuilt `libduckdb` dynamically (then `libduckdb.so` has to ship next to it) —
  useful when the executable size matters more than the single-file property.
* **One connection, one writer:** HTTP handlers never touch the database. They push parsed
  hits into an in-memory buffer; a dedicated writer thread drains the buffer and writes one
  transaction every **2 seconds**.
  *Why one connection:* DuckDB keeps the snapshot a connection started with — a second
  connection opened *before* a write keeps returning the old state, even read-write. So the
  server uses exactly **one** connection behind a mutex: the dashboard always sees fresh
  numbers, and there is a single writer (DuckDB's model anyway).
  The CLI opens the file read-only instead, where a snapshot is exactly what an offline
  analysis wants.
* **Back-pressure / overload:** if the buffer exceeds `MAX_QUEUE` (default 100 000), the
  service drops the **oldest** buffered hits and counts them (surfaced in `/api/heartbeat`),
  instead of growing until the OOM killer arrives. Statistics do not need to be perfect; the
  server staying up matters more. The log line for it is deliberately friendly:
  `[sumimi] the 33rd kitty was too cute to count, the server dozed off — later kittens can wait`.
* **Auth:** username + password (Argon2id), JWT (HS256) bearer tokens, like Umami's
  `POST /api/login` → `{ token, user }`. The admin account is created on the first run; the
  password is generated and printed **once** unless `SUMIMI_ADMIN_PASSWORD` is set.
* **TLS:** `--ssl <domain>` obtains a Let's Encrypt certificate over ACME (TLS-ALPN-01) and
  renews it in the background — no certbot, no cron, no reverse proxy needed just for TLS.
  Port 443 must reach the process directly; behind a TLS-terminating proxy, let the proxy
  manage the certificate instead.
* **Static hosting:** the binary serves `service/js/sumimi.js` (embedded at compile time via
  `include_str!`, ~7 KB) and the docs site from `service/docs/` — **embedded in the binary**
  by `service/build.rs` (~60 KB), with the on-disk copy taking precedence (`/docs`).
  The **dashboard frontend is not in the binary**: the shell at `/` points at a CDN
  (`SUMIMI_APP_URL`, default `https://cdn.jsdelivr.net/gh/sumimi-dev/sumimi@main/js`),
  which serves this public repository directly. Set `SUMIMI_APP_URL=/app` to serve it
  from the backend instead (air-gapped installs).

* **One builder for the traffic:** `service/src/testing.rs` has `HitBuilder` —
  the tests, the benchmarks and the memory budget all put events in the
  database with it, so there is one place that knows what a hit is made of.
  It is also in the public API on purpose: the private edition writes the
  same hits, and it must not have a second, slightly different idea of them.

### 4.1e The write path: the queue, the batch, and what the client is told

Three rules, and they are one idea: **what we acknowledged, we have.**

1. **The queue is bounded** (`SUMIMI_MAX_QUEUE`, 100 000 events). When it is
   full, the server **refuses** the new event — it does *not* drop an older
   one. The events in the queue were acknowledged to their clients; dropping
   one of those would lose data somebody believes they sent.
2. **The batch is capped** (`SUMIMI_MAX_BATCH`, 10 000 events). The batch is
   written in one transaction, and its rows sit in memory while it is
   written — so the cap is what keeps the write path's memory independent of
   the traffic rate. It also keeps the writer from holding DuckDB's single
   write connection for long: the dashboard's queries wait behind it.
3. **The client is told.** A refused event gets a **503** with a
   `Retry-After` — never a `200`. A `200` that hides a lost event is a lie,
   and the tracker retries on a `503` (the event was never counted, so
   sending it again cannot count it twice).

### 4.1b The shared query layer (`service/src/reports.rs`)

Every report — summary, timeseries, the ten breakdowns, sessions, events — has **exactly
one implementation**, in `service/src/reports.rs`:

```
service/src/reports.rs        ← the only SQL in the project
   ↑                ↑
service/api/*      cli/src/source.rs
```

The API handlers are thin wrappers around it, and the CLI's local mode calls the very same
functions. Two consequences:

* the numbers the CLI prints and the numbers the API returns are equal **by construction**;
* a metric that changes (say, how a bounce is counted) changes in one place.

Never write a second SQL query for a report that already exists here.

### 4.1c The storage trait (`service/src/storage.rs`)

Every report and every write goes through one trait, `Storage`. The open-source build
implements it once — for DuckDB, in `service/src/reports.rs`.

The trait defines the **semantics**, not the SQL: timestamps, date bucketing, distinct
counting and upsert syntax differ between engines, so each backend writes its own
dialect against the same contract. The private `sumimi-pro` repository adds SQLite and
PostgreSQL backends against this trait; the open-source build ships **only DuckDB**.

Consequences for this repository:

* the API handlers, the CLI and the writer all talk to `Arc<dyn Storage>` — never to
  `duckdb::Connection` directly;
* a report has exactly one implementation, so the dashboard and the CLI cannot disagree;
* the batch grouping (`storage::aggregate_batch`) is backend-agnostic — the backends only
  differ in how they write the result.

### 4.1d The two memory budgets

The product is specified against **two** memory budgets, and **every test runs
against both of them**:

| tier | the machine | the engine's ceiling | the **whole process** must stay under | threads |
| --- | --- | ---: | ---: | ---: |
| **minimum** | 1 vCPU / 512 MB | 128 MB | **256 MB** | 2 |
| **recommended** | 1 vCPU / 1 GB | 256 MB | **512 MB** | 4 |

**The number that is asserted is the process's peak RSS**, not the engine's own
usage. The engine's number (`duckdb_memoryBytes`) is only a part of what a small
box runs out of — the binary's code pages, the writer's queue, the pages the
allocator has not handed back, and the engine's share all count. So:

- on **Linux**, the peak is the exact high-water mark from `/proc/self/status`
  (`VmHWM`) — `service/src/rss.rs`;
- the assertion lives in the **Linux CI** (`.github/workflows/ci.yml`, the daily
  `budget` job), because macOS counts the shared pages of every mapped binary
  differently, so the number there is not the number a VPS sees.

The measurement is `service/src/memcheck.rs`: it pushes realistic traffic (the
same pages, the same visitors, the same referrers — uniform data would compress
into a dictionary and the measurement would be a lie) through the **real** write
path, and reports the peak RSS, what the engine holds, the write latency and the
size of the file. The tool is `service/examples/memory_budget.rs`:

```bash
cargo run --release --example memory_budget -- --engine 128MB   # the minimum
cargo run --release --example memory_budget -- --engine 256MB   # the recommended
```

The exit code is the assertion: **0** when the process stayed inside its budget.

### 4.2 Umami v2 API compatibility (the contract)

Implemented in v1 (target for the `sumimi-dev/sumimi` client used by the dashboard):

| Method | Path                                   | Purpose                                  |
| ------ | -------------------------------------- | ---------------------------------------- |
| POST   | `/api/send`                            | collect (`type`: event/identify/screenview) |
| POST   | `/api/collect`                         | collect (legacy query-parameter form)     |
| GET    | `/api/websites`                        | list websites                            |
| POST   | `/api/websites`                        | create website                           |
| GET    | `/api/websites/:id`                    | website detail                           |
| PUT    | `/api/websites/:id`                    | update website                           |
| DELETE | `/api/websites/:id`                    | delete website + its data                |
| GET    | `/api/websites/:id/stats`              | summary metrics                          |
| GET    | `/api/websites/:id/stats/metrics`      | metric breakdown                         |
| GET    | `/api/websites/:id/stats/timeseries`   | time series                              |
| GET    | `/api/websites/:id/stats/{pages,referrers,browsers,os,devices,countries,languages,screens,events,urls}` | breakdowns |
| GET    | `/api/websites/:id/sessions`           | session list                             |
| GET    | `/api/websites/:id/events`             | custom event list                        |
| POST   | `/api/login`, `POST /api/auth/refresh`, `GET /api/me` | auth            |
| GET    | `/api/heartbeat`                       | health + queue stats                     |
| GET/POST | `/api/logs`                          | the instance log                         |
| POST   | `/api/gsc/import`                      | a Search Console import (one transaction) |
| GET    | `/api/websites/:id/gsc`, `/api/websites/:id/gsc/imports` | the imported rows |
| GET/POST | `/api/snapshots`                     | the saved reports                        |
| POST   | `/api/snapshots/:name/run`             | run a saved report again                 |
| DELETE | `/api/snapshots/:name`                 | forget a saved report                    |
| GET/POST | `/api/chat/sessions`, `/api/chat/sessions/:id/messages` | the assistant's conversations |
| GET/DELETE | `/api/cache`                        | the report cache                         |

**Everything but the collector needs a token.** One middleware
(`api::require_auth`), so a new route cannot forget it: the collector
(`/api/send`, `/api/collect`), the tracker, the dashboard's own files and the docs are
public; everything else is refused with a `401` without a valid bearer token.

Out of scope for v1 (explicitly, so nobody "fixes" it by accident): teams, roles/permissions,
annotations, comments, share links, user management UI, plugin marketplace, realtime
(websocket), A/B testing, the Umami **dashboard front-end** (this project ships the *data
service*; the official Umami dashboard is expected to work against it, but we do not fork it —
see §6).

### 4.3 Data model: one fact table (DuckDB)

**Every page view is one row in `event`**, carrying the same fields Umami carries.
Sessions, visitors and page views are all *derived* from it — nothing is stored twice,
so nothing can drift apart, and the page attribution is exact.

```sql
event(
    id BIGINT,               -- monotonic, assigned by the writer, NOT a primary key
    website_id,
    created_at,

    visitor_id UUID,         -- the visitor: 16 bytes (schema v10)
    visit_id UUID,           -- the session: 16 bytes (schema v10)
    user_id UUID,            -- the visitor as the site owner knows them (nullable)
    session_start TIMESTAMP, -- when the session began: a fact, stored (schema v10)

    ip              VARCHAR,         -- the raw address (empty unless SUMIMI_STORE_RAW=1)
    ip_prefix       VARCHAR,         -- its coarse prefix (empty unless SUMIMI_STORE_RAW=1)
    user_agent      VARCHAR,         -- the raw user agent (empty unless SUMIMI_STORE_RAW=1)

    url, url_query, title,                       -- the page
    referrer, referrer_path, referrer_query,     -- where it came from
    referrer_name,                               -- "Hacker News" (schema v6)
    hostname,
    channel,                                     -- direct, Search, Social, email, paidAds, …

    browser, browser_version, os, os_version, device, screen, language,
    country, region, city,
    latitude, longitude,                         -- the coordinates (schema v6)

    utm_source, utm_medium, utm_campaign, utm_content, utm_term,

    gclid, fbclid, msclkid, ttclid, lifatid, twclid,   -- ad click ids

    lcp, inp, cls, fcp, ttfb,                     -- Core Web Vitals

    event_type, event_name, tag, data
)

revenue(                    -- what the owner earned, imported (not tracked)
    id BIGINT, website_id, visit_id, event_id,
    event_name, currency, revenue, created_at
)
```

**The visitor id is stored** (schema v10). It is `SHA-256(address ‖ user_agent ‖
website_id)`, first sixteen bytes — a pure function of the visitor's address, their
agent and the site. ⇒ ⇒ The address that goes in is **the whole IPv4 address, or the /64
of an IPv6 one** (a /24 would merge a whole office into one visitor; IPv6 rotates
everything after its /64). ⇒ ⇒ The reports count the visitors out of that id, so the
count is over a **fixed-size key**, not over a pair of strings — which was 2–6× the
cost at report time.

⇒ ⇒ **The raw address and the raw user agent are not stored by default**
(`SUMIMI_STORE_RAW=1` turns them on) ⇒ ⇒ the visitor id is already a hash of them, so
the readings (country, region, city, browser, system, device) are all that the reports
need. ⇒ ⇒ The default is the clean one and the switch is the site owner's.

**The session id is sixteen bytes** (schema v10): `SHA-256(visitor_id ‖ the second the
session began)`. ⇒ ⇒ **The start itself is not in the id** — sixteen bytes have no room
for it — so it is **stored beside it**, in `session_start` ⇒ ⇒ the start of a session is
a fact about what happened, and it is written down rather than worked out of a bucket.

⇒ ⇒ A session is cut by a **30-minute idle timeout**, made when the event arrives ⇒ ⇒
the state that decides it lives in the **client's signed token** (`x-sumimi-cache`, or
`x-umami-cache`) ⇒ ⇒ **the server keeps no state** ⇒ ⇒ a request without a token falls
back to the server's own in-memory cache of the recently-seen visitors.

⇒ ⇒ **There is no migration.** The shape of the ids changed, and the rows of an
older file cannot be carried over — so an older database is **replaced**: the old file
is set aside under its own version number (`sumimi.duckdb.v7.bak`) and an empty one of
the current version takes its place. The analysis data and the site registrations are
not carried over; the accounts are not in this file at all (they live in the instance
store), so they survive.

**The baseline is Umami.** We store everything Umami stores — plus the browser and OS
*versions*, which Umami does not keep. Two consequences worth stating plainly:

* **The visitor id survives across days** (Umami's `session_id`). It is a hash of the
  address and the user agent; the raw address is not stored unless `SUMIMI_STORE_RAW=1`.
* **The query string is stored**, because the UTM parameters and the ad click ids live
  in it. The server does the splitting — the browser sends the whole URL.

**Why one table and not a session table**: an earlier design kept one row per session
and counted the page views in a column. It was smaller and it was **wrong**: a session's
views were all attributed to the page it happened to end on. With one row per view the
attribution is exact by construction, the write is an append (the fastest path the
engine has), and the extra rows cost a fraction of what they save.

**Why there is no index on the fact table** (schema v3) — the key, in one line:

> **All write paths share one memory budget.** A table with a primary key needs a
> **~1 GB** budget for a bulk load; without one it needs **128 MB** — the same
> budget the server already runs in. An index on an append-only table would give
> the design a cliff: one path that works everywhere except on the customer's
> 512 MB box, and only at scale. On top of that it costs **8 bytes per row**
> (about 7% of a live database).

What the index does **not** buy, so nobody repeats the claim: **not** query speed
(the index is never in a plan — the 15 dashboard queries are 0.92–1.07x, pure
noise) and **not** steady-state memory (the peak RSS while serving is the same
with and without it, and it does not grow with the row count).

`event` and `revenue` are append only, and every query over them is a filtered
scan (`WHERE website_id = ? AND created_at BETWEEN ?`). DuckDB prunes those with
**zone maps** — one min/max per column per row group, maintained automatically and
stored with the data — so an index buys nothing at query time either. Measured
(`cargo run --release --example index_probe -p sumimi-service`):

| | with `PRIMARY KEY` | without |
| --- | ---: | ---: |
| disk, the real 42-column schema, 20000000 rows | +161 MB (**+8.05 B/row**) | — |
| disk, the real 42-column schema, 100000 views | +1.0 MB (**+10.5 B/浏览**) | — |
| append (the server's path) | +1.5% | — |
| peak RSS while serving (20000000 and 60000000 rows) | **the same** | — |
| a bulk `INSERT … SELECT` under a 128 MB limit | **out of memory** | 17.8 s, fine |
| the 15 dashboard queries | 0.92–1.07x — **noise** | — |

DuckDB 2.0 **persists** the ART index of a primary key to disk, and it enforces the
uniqueness constraint on every single row. The ids are handed out by the writer
(`MAX(id) + 1`, answered from the column statistics in constant time), so the
constraint protects nothing. The small dimension tables (`website`, `meta`, `user`)
keep their keys — they hold a handful of rows, and `meta` needs its primary key for
`ON CONFLICT` to work.

**What this is *not*.** Removing the primary key does **not** make queries faster
(the index was never in a plan) and it does **not** lower the memory the server
holds: the ART index of a *monotonic* key is a radix tree that collapses to almost
nothing in RAM — the peak RSS is the same with and without it, at 20M and at 60M
rows, and it does not grow with the row count. What it does buy:

* **8 bytes of disk per row** — about 7% of a live database;
* **one memory budget for every path.** Writing a lot of rows into a table that
  *has* a primary key needs a **~1 GB** memory limit (60000000 rows: out of
  memory at 128 MB, 256 MB and 512 MB; fine at 1 GB). Writing the same rows into
  a table without one needs **128 MB**. Chunking does not help — with a primary
  key the bulk load still runs out of memory, at the 13-millionth row.

  The schema migration is unaffected (it writes into the new table, which has no
  primary key): 60000000 rows at a 128 MB limit, 53 s. But a future *import* into
  a live database is not — that is the case this buys.

**What it does *not* save: steady-state memory.** While the server is serving, the
peak RSS is the same with and without the primary key (60000000 rows: 30.5 MB
either way; 20000000 rows: 258 MB without, 239 MB with — noise), and it does not
grow with the row count. The index of a *monotonic* key is a radix tree that
collapses to almost nothing in RAM; it costs 8 bytes per row **on disk**, not in
memory. So: **disk yes, steady-state memory no.** The memory difference is in
the *budget a bulk load needs* — with a primary key DuckDB has to be given ~1 GB
to get through it, and it will then use it.

**How many bytes a page view costs** (`cargo run --release --example row_cost -p
sumimi-service`, the real schema, 100000 views):

| | B/浏览 |
| --- | ---: |
| synthetic traffic (a handful of pages, mostly NULL columns) | 42.1 |
| realistic traffic (long URLs, titles, UTM, click ids) | **86.6** |
| the worst case (every URL, title and referrer unique) | 209.8 |

Those numbers use the short ids the benchmark scripts use. The **server** hands out
UUIDs, and the session and visitor id are stored in *every* row.

**Done (schema v4):** the ids are DuckDB's 16-byte `UUID` type, SQLite's 16-byte blob
and PostgreSQL's `uuid` — the same 16 bytes everywhere, and the API is unchanged (the
strings are only formatted at the edge). The measured cost of a live site drops from
**115.5 B/浏览** to about **105 B/浏览**, and the `COUNT(DISTINCT …)` hash tables in the
summary hold a third of the bytes.

### 4.3d The dimensions, and where the visitor is

Every way the traffic can be broken down is a **dimension** (`storage::Dimension`):

| dimension | column | counts |
| --- | --- | --- |
| pages, referrers, hosts, tags | `url`, `referrer`, `hostname`, `tag` | views |
| browsers, os, devices, countries, regions, cities, languages, screens, channels | the column of the same name | unique visitors |

**The channel** is worked out **once**, when the event is collected (`channels::channel`),
and stored with it: a visit from a search engine is a search visit, and the report does
not have to guess it from the referrer every time.

**The buckets are the reference implementation's, rule for rule** ✓ — `direct` ✓,
`paidAds` ✓, `referral` ✓, `affiliate` ✓, `sms` ✓, `llm` ✓, `Search` ✓, `Social` ✓,
`email` ✓, `Shopping` ✓, `Video` ✓ — so the same visit lands in the same bucket in
both, and the two sets of numbers can be compared ✓ ✓ (AGENTS.md §4.3e ✓).

Two refinements ✓: the **longest** domain match wins ✓ — `mail.google.com` is a
mail client ✓, not a search engine ✓ — and the mail clients the reference does not
have are in the list ✓ (Gmail is too big a referrer to get wrong ✓ ✓).

**The query parameters** are not a dimension: they are split out of the query string in
Rust (`storage::parse_query_parameters`), because the three engines do not agree on how
to split a query string and the answer has to be the same in all of them.

**A custom event is not a page view.** The pageview metrics — the summary's page
views, every breakdown, the time series, the query parameters — count only the
events that are page views (`event_type NOT IN ('event', 'identify')`). The
reference implementation does the same (`event_type NOT IN (2, 5)`), and so do we:
a site with a lot of custom events would otherwise show them as pages.

Two details follow from it, and both are the reference implementation's rules:

* the **visitors** and the **sessions** are counted over *everything* — a visitor
  who only fired a custom event is still a visitor;
* a **bounce** is a session with a single *page view* that did not fire a custom
  event — a session that only fired a custom event has no page view at all, and
  it is not a bounce.

The **logs** — the session list and the event list — keep every event: they are
logs.

**Where the visitor is** comes from one of three places, tried in this order:

1. **The header.** A proxy in front of the server knows the country exactly — it is the
   one that terminated the connection — and puts it in a header (`CF-IPCountry` and
   friends). `SUMIMI_COUNTRY_HEADER` says which header. This is what the open-source
   build uses: no data files, no database, nothing to keep up to date.
2. **The resolver** (`geo::Resolver`). The private edition has one: a MaxMind GeoLite2
   file, read into memory and asked on the collection path. It adds the region and the
   city.
3. **Nothing.** The country stays empty and the report says "unknown".

**The IP address itself is never stored** — only the country code, the region and the
city come out of the lookup.

### 4.3e The field set: whatever the reference implementation has, we have

**The rule** ✓: every field the reference implementation's event row carries,
we carry too ✓ ✓ — the coordinates ✓, the referrer's display name ✓, the
referrer's type ✓, everything ✓ ✓. **No field is dropped because it seems
unimportant** ✗ ✗ ✓.

Two things follow from it ✓:

- **A field is a capability, not a byte** ✓ — `browser` ✓, `country` ✓,
  `channel` ✓, `referrer_name` ✓: each one is a report the owner can run ✓ ✓.
  Merging two of them to save a dictionary index ✗ destroys a report ✓ ✗ —
  **the disk is managed by the retention policy, not by squeezing the row** ✓ ✓.
- **Filling the fields in makes our rows wider** ✓ — so the size advantage
  shrinks ✓ ✓. **That is the right trade** ✓ ✓: the fields are what the product
  is ✓ ✓; the disk is bounded by the retention window ✓ and the backups ✓ ✓.

**How "have them all" is proved** ✓ — not by reading the schema, but by
**writing the same data into both and comparing row by row** ✓ ✓ (the parity
suite in the private edition ✓ ✓): every field, every value ✓ ✓.

Our names differ from the reference implementation's ✓ — `visitor_id` for
`user_id` ✓, `url` for `url_path` ✓, `title` for `page_title` ✓, `referrer` for
`referrer_domain` ✓, and `channel` for `referrer_type` ✓ ✓ — but **the value
sets have to match** ✓ ✓ (the channel buckets: direct ✓, paidAds ✓, referral ✓,
affiliate ✓, sms ✓, llm ✓, Search ✓, Social ✓, email ✓, Shopping ✓, Video ✓ ✓).

**The fields the reference does not have** ✓ — the coordinates ✓, the referrer's
display name ✓ — we carry anyway ✓ ✓: a field is a capability ✓, and the rule is
a floor ✓, not a ceiling ✓ ✓.

### 4.3a The control plane: the instance store

**The events and everything else are different shapes, so they live apart.**

| | |
| --- | --- |
| **the data plane** (the events) | append-only, read as aggregations — a column store is right for it |
| **the control plane** (the rest) | small, mutable, transactional — a row store is right for it |

The control plane is one small SQLite file (`instance.sqlite`, next to the events) with
eleven tables: the accounts, the teams, the sign-in attempts, the log, the Search
Console import, the report cache, the saved reports and the assistant's
conversations.

**It is a trait** (`service/src/instance.rs`): `Instance` has 22 methods, and the private
edition implements the same trait in PostgreSQL (`sumimi-pro`'s
`instance::PostgresInstance`). The API layer does not know which one it is talking
to, and `pro/tests/instance.rs` checks that both keep the same numbers.

**Why it matters:** the accounts survive an analytics rebuild (rebuilding the DuckDB
file is a write-new-file-and-swap — the accounts are not in it), and the whole control
plane is a few hundred kilobytes you can back up in a second.

### 4.3b DuckDB has one connection, and that is deliberate

**DuckDB snapshots the database when a connection is opened.** A connection that
queried before a write never sees that write — not even after a checkpoint. And a
connection opened *after* the write sees it only until the older connection is closed.
It was measured, not guessed: `cargo run --release --example snapshot_probe -p
sumimi-service`.

```
1. the reader queried before the write: 0
   the writer sees:                    1
   the reader still sees:               0
2. a connection opened after the write: 1
3. after a checkpoint:                  1
   after the old connection is closed:  0
   after everything is closed:          1
```

So the DuckDB backend uses **one connection**, shared by the writer and the dashboard.
A second connection would show the dashboard stale numbers, and the numbers would
change under the reader's feet.

**The other two engines do not have this problem** and do use a pool of readers:
SQLite in WAL mode gives every read a fresh snapshot, and PostgreSQL has had proper
MVCC for thirty years.

**The price is that the requests are serialised** — which is why the **report cache**
matters (below): the dashboard's repeated questions are answered without touching
DuckDB at all.

### 4.3c The report cache

The report cache lives in the instance store, keyed by the report and everything that
changes its answer. **Every cached answer carries the version of the data it was
computed from**, and the writer bumps that version with every batch — so a cached
answer from before the last write is a miss, not a stale hit.

The dashboard's numbers are therefore never older than the last flush, and two people
looking at the same dashboard only run the query once.

**One place decides what a report is**: `api::stats::run_report`. The stats endpoints,
the saved reports (snapshots) and the cache all go through it, so they cannot disagree
about what "the top pages" means.

### 4.4 cli/ — the analyzer (two sources, one UX)

The CLI (`sumimi-client`, or `sumimi` in its client mode) works against **either** a local
database file **or** a running service, and the subcommands are identical in both modes:

```
sumimi websites                     # list websites
sumimi summary   --website <id>     # pageviews, visitors, views, bounces, duration
sumimi pages     --website <id>     # top paths
sumimi referrers --website <id>     # traffic sources
sumimi browsers  --website <id>     # browser / OS / device / country breakdowns
sumimi events    --website <id>     # custom events
sumimi sessions  --website <id>     # session log
sumimi timeseries --website <id>    # hourly/daily series
sumimi export    --website <id> --format json|csv
sumimi prune     --website <id>     # enforce retention   (local only)
sumimi erase     --website <id>     # delete a website's data
```

**Source selection** (exactly one of):

| Mode       | Flags                                                          | Notes |
| ---------- | -------------------------------------------------------------- | ----- |
| local file | `--db <path>` (default `./sumimi.duckdb`)                       | opened `access_mode=READ_ONLY`; `prune`/`erase` open it read-write |
| HTTP API   | `--url http://host:3000` + `--token <jwt>` or `--username/--password` | logs in as **admin** via `POST /api/login`, caches the JWT in `~/.config/sumimi/credentials.json` (mode `0600`) |

The **admin account** is the service's own user table (Argon2id password, JWT issued by
`POST /api/login`); it is the DB entry point for management operations. The first `serve`
on an empty database creates the admin user:

* `SUMIMI_ADMIN_USER` (default `admin`)
* `SUMIMI_ADMIN_PASSWORD` — if unset, a strong password is generated and printed **once**
  to the service log; it is never stored in plaintext.

The CLI **must never** open a local database for writing except in `prune`/`erase`.
In HTTP mode the equivalent destructive operations go through the authenticated API.

Flags common to all subcommands: `--from`, `--to` (ISO date or relative like `7d`),
`--json`, `--limit <n>` (default 50), `--no-color`.

### 4.4a cli/src/mcp.rs — the MCP server

`sumimi mcp` speaks the **Model Context Protocol**: JSON-RPC 2.0 over stdio, one
message per line. It is the same program as the rest of the CLI — the same
reports, the same analyst — with three tools:

| tool | what it does |
| --- | --- |
| `sumimi_report` | run a report (the summary, the top pages, the breakdowns, the sessions, the time series) |
| `sumimi_record` | record a page view or a custom event |
| `sumimi_ask` | ask a question and get the answer computed from the database |

**There are no write tools.** Nothing creates a website, changes a setting, prunes
data or erases anything. The server can look, count and record — recording is the
one thing that writes, because it is what the tracker does. (This is a scope
decision, not an omission: see §6.)

The rules:

* **stdout belongs to the protocol.** Nothing else may be printed there; diagnostics
  go to stderr. A stray `println!` breaks every client.
* **The numbers come from the same code the dashboard uses** — `reports` and
  `analyst` — so the agent and the owner cannot see different numbers.
* **A tool that fails is not a broken protocol**: the failure comes back as the
  content of the tool result with `isError: true`, so the model can read it. Only
  protocol problems (an unknown method, an unknown tool) are JSON-RPC errors.
* The protocol version the client asks for is echoed back when it is one this
  server knows (`2025-06-18`, `2025-03-26`, `2024-11-05`); otherwise the newest
  one is returned and the client decides.
* In HTTP mode the questions go to `POST /api/websites/:id/ask` — the analyst
  without a conversation. In local mode the analyst is called in-process.

### 4.5 js/ — the tracker and the dashboard frontend

Two independent, **dependency-free** files (no build step, no npm, no bundler):

**`service/js/sumimi.js` — the tracker** (embedded in the binary, served at `/sumimi.js`):

* sends pageviews and custom events to `/api/send` via `sendBeacon` (fallback `fetch`
  with `keepalive`), as `application/json`;
* is self-hosted by default (`<script defer src="https://sumimi.example/sumimi.js"
  data-website-id="...">`) — **the service serves it from its own origin**;
* sets no cookies, uses no `localStorage`, honours DNT/GPC;
* reports **Umami's full field set**: the website, the full URL and referrer, the title,
  the hostname, the language, the screen, the event name, tag and data, and the five web
  vitals (`lcp`, `inp`, `cls`, `fcp`, `ttfb` — sent as strings, the server parses them);
* sends the query string: the server parses the UTM parameters and the ad click ids
  (`gclid`, `fbclid`, `msclkid`, `ttclid`, `lifatid`, `twclid`) out of it;
* measures the web vitals with `PerformanceObserver` and sends the page view on
  `visibilitychange`/`pagehide` (the metrics are only known after the page is painted);
* patches `history.pushState`/`replaceState` and listens for `popstate`, so a single page
  application records every route change as its own page view;
* exposes `window.sumimi.track(name, props)`, `window.sumimi.identify(data)` and
  `window.sumimi.pageview()` (with `window.umami` as an alias for existing snippets);
* is byte-for-byte reproducible: the file in the repo is the file that gets served.

**`js/sumimi-app.js` + `js/sumimi.css` — the dashboard frontend** (served from disk at
`/app/*`, or hosted on a CDN and pointed at with `SUMIMI_APP_URL`):

* vanilla ES2020, no framework, no chart library (the timeseries chart is hand-drawn SVG);
* logs in against `/api/login` and keeps the admin token in `localStorage` — this is the
  *dashboard's own* credential and is unrelated to the tracker, which stores nothing;
* the backend only ever serves a tiny HTML shell that references these two files, so the
  frontend can be updated (or moved to a CDN) without touching the binary.

Deployment recipes (plain `<script>`, self-hosting, CDN, multi-site) live in `js/README.md`.

### 4.5a Hot and cold: Parquet is the archive, DuckDB is the working set

The server holds an **exclusive lock** on the DuckDB file, so the database cannot be
copied while the service runs — and copying a 400 GB file is not a backup strategy
anyway. The cold tier is therefore **Parquet**, partitioned by day:

```
archive/
  sessions/day=2026-11-03/data_0.parquet
  events/day=2026-11-04/data_0.parquet
  websites.parquet
```

* `sumimi backup --out ./archive` writes it — **one pass per table**
  (`COPY … TO … (FORMAT PARQUET, COMPRESSION ZSTD, PARTITION_BY (day))`).
* `sumimi --parquet './archive/*' summary <id>` reads it **in place**: the archive is
  queried through `read_parquet()`, never imported, so analysing years of history costs
  nothing but the query.
* The archive is self-describing (Hive-style partitions) — DuckDB, Spark, Polars and
  pandas can all read it without knowing our layout.
* The **same report code** runs over both tiers, so the numbers are identical by
  construction. `cli/tests/archive.rs` asserts exactly that.

Rule: the hot tier holds the last N days (14–90), the cold tier holds everything older.
A 20 GB disk plus a 90-day hot window plus daily roll-ups covers ten years of history.

### 4.5b Demo data (`sumimi seed`)

`sumimi seed --db <file> --days 30 --visitors 140` fills a database with **synthetic**
traffic: a weighted page/referrer/country/browser distribution, returning visitors within
a day, weekend dips, and custom events. It exists so the dashboard can be evaluated and
demoed without pointing a real site at it.

Everything it generates is invented (invented domains, invented visitors). It writes
through the storage backend (`--backend duckdb` or `--backend sqlite`), so the same
traffic lands in either one — and the server has to be stopped while it runs (DuckDB
takes an exclusive lock on the file).

**The seed's random numbers must not be zero.** The generator xorshift-mixes the seed
with a constant, and xorshift on zero stays zero — every session came out identical
(one view, one event each). The seed constant and the mixing constant must stay
different.

### 4.6 service/docs/ — the website

A static site (plain HTML + one stylesheet, no build step, no npm) served by the service
binary at `/docs`. **The docs directory is the root of the site**, and every page is an
`index.html` in a directory of its own, so a page's address is its directory:

| Address | The file | What it is |
| ------- | -------- | ---------- |
| `/` | `service/docs/index.html` | The home page: the story (why the project exists), the design goals with the measured numbers, and the way in. |
| `/docs/` | `service/docs/docs/index.html` | The documentation section: the list of the articles. |
| `/install/` | `service/docs/install/index.html` | Installation and quick start. |
| `/self-host/` | `service/docs/self-host/index.html` | Self-hosting on a cheap VPS. |
| `/tracker/` | `service/docs/tracker/index.html` | The tracker and its deployment options. |
| `/api/` | `service/docs/api/index.html` | The HTTP API. |
| `/cli/` | `service/docs/cli/index.html` | The command line. |
| `/memory/` | `service/docs/memory/index.html` | The memory promise and the deployment reference. |
| `/privacy/` | `service/docs/privacy/index.html` | Privacy and data protection. |
| `/faq/` | `service/docs/faq/index.html` | Frequently asked questions. |
| `/blog/` | `service/docs/blog/index.html` | The blog: the stories, the case studies and the data. |

Every documentation page carries the same left-hand list of articles. Beside them:
`assets/style.css` (one stylesheet, light + dark via `prefers-color-scheme`),
`assets/og.png` (the social card), `assets/CREDITS.md` (the licence record for every
asset — all artwork is original), `sitemap.xml` and `robots.txt`.

**Every link is relative, and that is not a style choice.** The same files are published at
`https://sumimi.dev` (the docs directory is the site root) and served by the
binary at `/docs` (the docs directory is one level down). A relative link resolves the same
way in both; an absolute one does not. So the stylesheet is `../assets/style.css` from a
page in a directory, and a page is linked as `../install/` from another one.

**The SEO of the site is part of it.** Every page carries a title, a description, a
canonical URL, the Open Graph and Twitter tags, and its structured data (JSON-LD); the
sitemap lists exactly the pages that exist. There is no build step to keep them in step —
`service/tests/docs.rs` does it instead: the test fails when a page has no canonical URL,
when there is more than one `<h1>`, or when the sitemap and the pages disagree.

The site lives **inside the crate** and is **embedded in the binary** by `service/build.rs`
(about 60 KB): a server installed with `cargo install` has a documentation site without
anything else to deploy. The on-disk copy still wins at runtime — the release archive
ships the documentation next to the executable, and a user may have edited it.

Docs are plain HTML on purpose: no Markdown renderer has to be shipped in the binary, and
the pages work when opened straight from the filesystem.

The **dashboard** is separate from the docs: the backend serves a very small HTML shell at
`/` that loads `js/sumimi-app.js` + `js/sumimi.css` (see §4.5). Point `SUMIMI_APP_URL` at a
CDN to serve that frontend from anywhere; the backend binary stays tiny because the
frontend is never embedded in it.

### 4.7 The service, and the container

**`sumimi service start|stop|status|restart`** runs the server under a **supervisor**: a
second process that owns the server as a child and restarts it when it dies. The rules it
follows, and the reasons for them:

* **Two processes.** A process the kernel kills — the OOM killer, a segfault — cannot
  restart itself. The supervisor is a separate executable for exactly that reason.
* **No system manager, no root.** Everything the service owns lives inside the instance
  directory, so it runs as the user who installed it. The supervisor is started in its own
  process group (`CommandExt::process_group`) so that closing the terminal does not take it
  down.
* **No signals.** The supervisor asks the server to stop by closing the pipe on its
  standard input; the server treats the end of that pipe as a request to shut down
  gracefully (`SUMIMI_EXIT_ON_STDIN_EOF=1` — opt-in, because with a terminal on standard
  input an end-of-file would be a surprise). The stop request is a file, and so is the
  state, so `status` works without any privileges.
* **Failure protection.** A server that dies within ten seconds of starting is a broken
  server: after five of those in a row the supervisor stops and says so, rather than
  restarting it forever. The backoff between restarts grows to thirty seconds — and a stop
  request interrupts it.

The state lives in `<dir>/service/`: `status.json` (the heartbeat), `log` (the server's
output) and `stop` (the stop request).

**The container** (`Dockerfile`, `docker-compose.yml`) is the same bundle in a box: the
executable, the dashboard frontend and the documentation, exactly as the release archive
ships them. It runs as an unprivileged user, serves the frontend itself
(`SUMIMI_APP_URL=/app`, so the container makes no outbound request) and reads **its own**
memory limit rather than the host's. `--restart unless-stopped` is the container's answer
to the same job the supervisor does outside Docker.

**TLS is the server's own job by default.** With a domain, the server asks Let's Encrypt
for a certificate over ACME (TLS-ALPN-01) and renews it in the background — no certbot, no
renewal cron job, no reverse proxy whose only job is TLS.

## 5. Non-negotiable engineering rules

1. `#![forbid(unsafe_code)]` in every crate.
2. **No new runtime dependencies** without a comment in the PR explaining why. The dependency
   budget is part of the product's selling point (single binary, tiny RAM).
3. **No telemetry, no outbound requests, no analytics of our own.**
4. All user-visible strings are English (the OSS audience) and are documented in both
   `en` and `zh-CN` where the docs have a translation.
5. Every public API route has an integration test; every DuckDB query in `cli/` has a test
   against a generated fixture database.
6. `cargo test` and `cargo clippy -- -D warnings` must pass before push.
7. **Every secret comes from the operating system's random source.** The JWT signing key,
   the generated admin password and the password salt all go through
   `config::random_secret` / `config::random_bytes` — and the shim behind them is small
   enough to be wrong in a way nothing notices. `service/tests/secrets.rs` guards it: a
   secret that is not random is a secret that everybody knows.
8. Schema changes are **additive** and must not require a migration step in v1
   (`CREATE TABLE IF NOT EXISTS` at open time).
9. **Every write path must fit in the same memory budget as the serving path.**
   The target box has 512 MB and the DuckDB ceiling is 128 MB, so a path that
   needs more is a path that breaks on the customer's machine. This is the reason
   the fact table carries **no index at all** — see §4.3. A table with a primary
   key needs a **~1 GB** budget for a bulk load (60000000 rows: out of memory at
   128 MB, 256 MB and 512 MB; chunking does not help — it dies at the
   13-millionth row), while the same rows without one need 128 MB.
   *Check this before adding a write path:*
   `cargo run --release --example idx_memory -p sumimi-service -- <rows> <pk|nopk> <path> <limit>`.

---

## 6. What we are *not* building (scope guard)

* **A fork of the Umami dashboard.** We implement the API; the community dashboard is used
  as-is. Building our own dashboard is a v2+ project (and belongs in `docs/` only as an
  optional static UI).
* Anything requiring a server larger than 1 vCPU / 512 MB RAM for the base install.
* Anything requiring a managed cloud service to function.

---

## 7. Development workflow

### 7.1 Where the tests live

| layer | what it checks | where |
| --- | --- | --- |
| **basic** | the functions: every endpoint, every report, the collection path, the write path, the migrations | **this repository** |
| **feature** | the private edition's behaviour: PostgreSQL, object storage, the jobs, MCP | `sumimi-dev/sumimi-pro` |
| **scale** | the invariants that break a customer's small box: the memory budget, the missing primary key, the useless index | `sumimi-dev/sumimi-pro` |
| **comparison** | against other implementations (Umami and the like) | `sumimi-dev/sumimi-pro` |

**This repository carries the basic layer.** It has to run on every pull request,
in minutes, on anybody's machine — and it has to be readable: the tests are the
manual. The heavy layers live in the private edition, where the code that
produced the numbers is not shipped.

**What is measured can still be published.** When a number is worth stating
publicly — a release note, a conclusion, a comparison — the report is written
into this repository (`benchmarks/<date>.md`). The benchmark *code* stays
private; the *result* does not have to.

**The measuring tool is public.** The load generator and the probe live in
[`sumimi-dev/sumimi-gotest`](https://github.com/sumimi-dev/sumimi-gotest) — Go,
the standard library only, one static file per platform, so it runs on a
machine that has nothing on it but the server. It is a **second implementation**
of the API on purpose: a probe that shares code with the server cannot
disagree with it, and a probe that cannot disagree cannot find anything. What
stays private is the *heavy* work: millions of rows, and the comparison with
other implementations.

### The two CI paths

`.github/workflows/ci.yml`:

| job | when | what it runs |
| --- | --- | --- |
| **The functional tests** | every push and pull request | `cargo test --workspace` + the tracker's tests — the basic layer, in minutes |
| **Clippy and rustfmt** | every push and pull request | `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` |
| **The memory budget** | **daily** and by hand | the two budgets on Linux (§4.1d): the peak RSS, the engine's usage and the write latency are recorded in the run summary, and the job goes red when the process outgrows its box |

The heavy layers — millions of rows, the comparison with other implementations —
are **not** in this repository's CI: they live in the private edition's, and they
run there.

See [issue #4](https://github.com/sumimi-dev/sumimi/issues/4) for the full
statement and the remaining work.

```bash
make build            # debug build
make release          # release build (static DuckDB: self-contained binaries)
make test             # unit + integration tests
make lint             # clippy -- -D warnings
make run              # serve on :3000 with ./dev.duckdb
make package          # release artefact: the `sumimi` binary + js/ + docs/
```

Plain cargo works too:

```bash
cargo build --release
cargo test --workspace
cargo run -p sumimi-cli -- serve --db ./dev.duckdb --port 3000
cargo run -p sumimi-cli -- --db ./dev.duckdb summary <website-id>
```

The first build compiles DuckDB from source — that is the slow part (set
`CARGO_HTTP_TIMEOUT=600` on a slow link).

Local smoke test:

```bash
curl -X POST http://localhost:3000/api/send \
  -H 'content-type: application/json' \
  -d '{"type":"event","payload":{"website":"<website-id>","url":"/hello"}}'
```

Note: Umami v2 sends pageviews as `type: "event"` **without** a `name`; a custom event
carries a `name`. The server treats a nameless event as a pageview.

Deployment (cheap VPS, Debian/Ubuntu):

```bash
# The release is a single self-contained binary — nothing else to install.
sudo install -m 0755 target/release/sumimi /usr/local/bin/
sudo systemctl enable --now sumimi
```

Optional aliases (symlinks, zero cost):
`ln -s /usr/local/bin/sumimi /usr/local/bin/sumimi-server`

With TLS managed by Sumimi itself:

```bash
sumimi-server serve --db /var/lib/sumimi/sumimi.duckdb \
                    --ssl example.com --ssl-email you@example.com
```

## 8. Version roadmap

* **v1 (current, implemented):** collection API, stats API, auth, DuckDB writer, CLI reader
  (local + remote), tracker, dashboard frontend, docs site, ACME TLS, one-binary deploy.
* **v1.1:** automatic retention pruning on a schedule, more breakdowns (UTM, campaigns),
  Prometheus metrics endpoint, HTTP mode for `prune`/`erase` in the CLI.
* **v2:** optional cold-tier (Parquet archive), multi-tenant teams, our own dashboard,
  distributed mode (ClickHouse) for the hosted SaaS tier.

---

## 9. Related private material

Internal decision log, unreleased plans and private discussion live in the **private**
`sumimi-dev/mneme` repository (GitHub issues + notes). Agents working on Sumimi may record
decisions there, but must never copy private content into this repository. When a decision
becomes public, move the *conclusion* (not the discussion) into this file.
