# Sumimi (数咪咪)

> **Count kitties popping into my websites.**

Sumimi is a **self-hosted web analytics service** that fits on a cheap VPS: one small
binary, one directory, no external database, no cloud account, no telemetry.

> It was called **sumami** until v0.2.0. The old tracker URL (`/sumami.js`), the old
> environment variables (`SUMAMI_*`) and the old crates all still work — but the name
> is Sumimi now.

- **Rust + Axum + Tokio** — one self-contained binary, ~50 MB of RAM at rest.
- **Two memory budgets** — it runs on a 512 MB box (the whole process stays under
  256 MB) and it is comfortable on a 1 GB one (under 512 MB). Both budgets are
  asserted on every CI run.
- **DuckDB 2.0.0** embedded — columnar analytics in a single file you can copy around.
  SQLite is there too, for the teams that want an ordinary database file.
- **Umami v2 compatible API** — the existing trackers and dashboards work unchanged.
- **Privacy first** — no cookies, no fingerprinting, **raw IP addresses are never stored**,
  DNT and Global Privacy Control are honoured, and the query string of collected URLs is
  stripped by default.
- **One command to deploy** — `sumimi serve --dir ./sumimi --port 3000`.

An open-source, high-performance alternative to Umami.

## The repository

| Path       | What it is                                                        |
| ---------- | ----------------------------------------------------------------- |
| `service/` | The server: collection API, stats API, auth, the writer, TLS.     |
| `cli/`     | `sumimi` — read and analyse a database, locally or over HTTP.     |
| `sumimi/`  | The facade crate: the server and the client as one library.       |
| `tracker/` | The tracker package — `@sumimi/tracker` on npm, `@ljh/sumimi` on JSR. |
| `js/`      | The dashboard frontend. The tracker is in `service/js/`.            |
| `docs/`    | The website and documentation — in `service/docs/`, so the binary carries them. |

[`AGENTS.md`](AGENTS.md) is the design document: architecture, API contract, privacy
rules, and the conventions every change has to follow.

## The tracker, typed

```bash
deno add jsr:@ljh/sumimi         # or: npm install @sumimi/tracker
```

```ts
import { init } from "@sumimi/tracker";

const sumimi = init({ website: "<website-uuid>" });
sumimi.track("signup", { plan: "pro" });
```

The tracker itself is still one file of plain JavaScript, served by the server
at `/sumimi.js` — the package injects it and hands the calls over.

## The analytics, as tools an agent can call

```bash
sumimi mcp --url http://127.0.0.1:3000 --token '...'   # or: --db ./sumimi/data.duckdb
```

`sumimi mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio, with **three tools**: `sumimi_report` (the numbers), `sumimi_record`
(log an event) and `sumimi_ask` (ask a question in plain language). There are
**no write tools** — nothing creates a website, changes a setting or erases
anything.

## Install

Download the release archive (it ships the dashboard frontend and the
documentation next to the executable):

| platform | file |
| --- | --- |
| macOS (Apple Silicon) | `sumimi-v0.4.1-Darwin-arm64.tar.gz` |
| macOS (Intel) | `sumimi-v0.4.1-Darwin-x86_64.tar.gz` |
| Linux (x86-64) | `sumimi-v0.4.1-Linux-x86_64.tar.gz` |
| Linux (ARM64) | `sumimi-v0.4.1-Linux-aarch64.tar.gz` |
| Windows (x86-64) | `sumimi-v0.4.1-Windows-x86_64.zip` |
| Windows (ARM64) | `sumimi-v0.4.1-Windows-aarch64.zip` |

```bash
mkdir sumimi && cd sumimi
curl -LO https://github.com/sumimi-dev/sumimi/releases/download/v0.4.1/sumimi-v0.4.1-Linux-x86_64.tar.gz
tar xzf sumimi-v0.4.1-Linux-x86_64.tar.gz
./sumimi serve --dir ./sumimi --port 3000
```

On Windows, unzip the archive and run `sumimi.exe serve --dir .\sumimi --port 3000`.

Or from crates.io — **the crates are published for the 0.3.x line**, so
`cargo install sumimi` gives that one, not the release above:

```bash
cargo install sumimi            # the server and the client, one binary
sumimi serve --dir ./sumimi --port 3000
```

The crates are `sumimi` (the binary), `sumimi-service` (the library) and
`sumimi-cli` (the client). They are Apache-2.0.

## Quick start

```bash
git clone https://github.com/sumimi-dev/sumimi
cd sumimi
cargo build --release          # the first build compiles DuckDB: be patient
```

The result is **one** executable in `target/release/`: `sumimi`. It is both the server and
the client — `sumimi serve …` starts the server, `sumimi summary …` reads the data —
because both halves share the schema, the metric definitions and the query code.

DuckDB is compiled into the executable, so the result is a **single file** — no shared
library to install, no `LD_LIBRARY_PATH`, no runtime surprises:

```bash
./target/release/sumimi serve --dir ./sumimi --port 3000
```

An instance is a **directory**: the events, the accounts and the rest live inside it,
so backing it up, moving it and deleting it are one operation on one path.

```
sumimi/
├── data.duckdb      the events
├── instance.sqlite  the accounts, the log, the imports, the cache
├── logs/            the server's log files
└── jwt-secret       the signing secret
```

The first run creates the admin account and prints its password **once**. Then:

```bash
# create a website
TOKEN=$(curl -s -X POST http://localhost:3000/api/login \
  -H 'content-type: application/json' \
  -d '{"username":"admin","password":"<the printed password>"}' | jq -r .token)

curl -s -X POST http://localhost:3000/api/websites \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"name":"My Blog","domain":"example.com"}'
```

Put the tracker on your site:

```html
<script defer src="http://localhost:3000/sumimi.js" data-website-id="<website-id>"></script>
```

Open <http://localhost:3000/> for the dashboard, <http://localhost:3000/docs/> for the docs.

## HTTPS without a reverse proxy

```bash
sumimi serve --dir /var/lib/sumimi \
             --ssl example.com --ssl-email you@example.com
```

The server obtains a Let's Encrypt certificate over ACME and renews it on its own.
The challenge is TLS-ALPN-01, so port 443 has to reach Sumimi directly; behind a
proxy that terminates TLS, let the proxy manage the certificate instead.

## The CLI

```bash
sumimi websites                              # local database
sumimi summary <website-id> --from 7d       # or --url https://analytics.example.com
sumimi pages <website-id> --from 24h
sumimi export <website-id> pages --format csv
sumimi gsc import gsc.csv --website <id>    # a Search Console export
```

The same commands work against a local `.duckdb` file and against a running server
(the admin account is used to log in). See `docs/cli.html`.

## Development

```bash
cargo test                                  # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo run -p sumimi -- serve --dir ./dev --port 3000
cargo run -p sumimi -- --db ./dev/data.duckdb summary <website-id>
```

`AGENTS.md` §7 lists the whole workflow.

## What is in the release

```
sumimi             one binary: the server (`sumimi serve`) and the client (`sumimi summary`)
js/                the tracker and the dashboard frontend (published on a CDN)
docs/              the documentation site
```

The binary is self-contained (DuckDB is compiled in) — copy one file, run it.
The dashboard frontend is served from a CDN by default, so the backend never ships it.

## Try it

```bash
sumimi serve --db ./demo.duckdb --port 3000   # prints the admin password once
sumimi seed --db ./demo.duckdb --days 30       # synthetic traffic (stop the server first)
```

Open <http://localhost:3000/> and sign in.

## Licence

**Apache-2.0** — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

Apache-2.0 rather than MIT because it grants an explicit **patent licence** to the
contributors, and because it is a licence a company's legal department already knows.
The versions published before 2026-11-04 were MIT; that grant stands for them, and
everything since is Apache-2.0.

Third-party software used by Sumimi keeps its own licence: DuckDB (MIT),
Axum / Tokio / Hyper (MIT / Apache-2.0), rustls (Apache-2.0 / ISC / MIT).
