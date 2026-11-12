# Credits and licences — `service/docs/`

This file is the licence record for everything in the `service/docs/` directory of the
Sumimi repository, as required by the project's design document (AGENTS.md §3).

## Summary

Everything shipped in `service/docs/` is **original work** created for the Sumimi project
and is covered by the project's own MIT licence (`LICENSE` at the root of the
repository). No third-party artwork, photographs, icon sets, fonts, CSS
frameworks, JavaScript libraries or tracking scripts are bundled here.

The documentation site is deliberately dependency-free: it uses no build step,
no npm, no bundler, no CSS framework and no web fonts. Every page is plain
static HTML plus one shared stylesheet, so the site loads fast and works
completely offline.

## Files in this directory

| File | What it is | Licence |
| ---- | ---------- | ------- |
| `style.css` | The single shared stylesheet for every documentation page. Hand-written; light and dark themes via `prefers-color-scheme`; responsive layout. | MIT (original work) |
| `logo.svg` | The Sumimi paw logo — the canonical source of the brand mark. Four toes and a pad, drawn by hand; no strokes, no gradients, no filters. Every other copy in the repository (the documentation pages, the dashboard frontend, the favicon) is inlined from this file and kept in sync by `service/tests/logo.rs`. | MIT (original work) |
| `CREDITS.md` | This file. | MIT (original work) |

The HTML pages live one directory up, in `service/docs/` itself. Every page is
an `index.html` in a directory of its own, so the address of a page is the
directory — the site is served from the root:

| Address | What it is | Licence |
| ------- | ---------- | ------- |
| `/` | The home page: the story of why the project exists. | MIT (original work) |
| `/docs/` | The documentation section: the list of the articles. | MIT (original work) |
| `/install/` | Installation and quick start. | MIT (original work) |
| `/self-host/` | Self-hosting on a cheap VPS: the service, TLS/ACME, backups. | MIT (original work) |
| `/tracker/` | Tracker installation and deployment options. | MIT (original work) |
| `/api/` | HTTP API reference. | MIT (original work) |
| `/cli/` | `sumimi` CLI reference. | MIT (original work) |
| `/memory/` | The memory promise and the deployment reference. | MIT (original work) |
| `/privacy/` | Privacy and data-protection page. | MIT (original work) |
| `/faq/` | Frequently asked questions. | MIT (original work) |
| `/blog/` | The blog: the stories, the case studies and the data. | MIT (original work) |

The `og.png` above is the picture a social card shows: the same mark, drawn
from the same SVG, on the same paper colour.

## Artwork

**All artwork in `service/docs/` is original.**

The paw prints and the cat-and-chart illustration are hand-written inline SVG
embedded directly in the HTML markup. They were drawn for Sumimi as simple
geometric shapes (ellipses and paths). No character, sprite, photograph,
illustration, logo or icon from any existing franchise, anime, film, game,
cartoon or other copyrighted work was used, traced, sampled or adapted.

The only other graphic element is the favicon, which is an inline SVG data URI
containing a single text glyph of a paw sign in the page's own `<head>`. It
requires no additional file and no external asset.

The user-interface icons used in the pages (shield, database, terminal, clock,
list, download, checkmark and similar) are likewise hand-written inline SVG
built from basic strokes and paths.

## Fonts

**No fonts are bundled, downloaded, embedded or referenced.**

The site uses **system font stacks** only, declared in `service/docs/assets/style.css`:

```css
--font-sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto,
  "Helvetica Neue", Arial, "Noto Sans", "PingFang SC", "Hiragino Sans GB",
  "Microsoft YaHei", sans-serif;
--font-mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas,
  "Liberation Mono", "Courier New", monospace;
```

These resolve to fonts already installed on the visitor's own device. Because
no font file is fetched, the site makes no third-party requests and works with
no network access at all. The named families are only fallbacks after the
generic `ui-*`/`system-ui` keywords and the generic `sans-serif`/`monospace`
families; none of them is redistributed here, so their licences do not apply to
this repository.

## Third-party software referenced (not bundled)

The documentation *names* the following third-party projects, because Sumimi is
built on or compatible with them. None of their code, binaries, assets or
artwork is included in `service/docs/`. The names are used nominatively, to identify
compatible and comparable products.

| Project | Role | Licence |
| ------- | ---- | ------- |
| [DuckDB](https://github.com/duckdb/duckdb) | The embedded analytics database. Version 2.0.0. The `libduckdb.so` / `libduckdb.dylib` shared library is distributed in the **release archive**, not in `service/docs/`. | MIT |
| [Axum](https://github.com/tokio-rs/axum) | The HTTP framework and router used by the `sumimi-server` service. | MIT |
| [Tokio](https://github.com/tokio-rs/tokio) | The asynchronous runtime used by the service. | MIT / Apache-2.0 |
| [Umami](https://github.com/umami-software/umami) | The analytics product whose **v2 HTTP API** Sumimi implements for compatibility. The official Umami dashboard is expected to work against a Sumimi server. | MIT (Umami itself; no Umami assets are used in `service/docs/`) |
| [Caddy](https://github.com/caddyserver/caddy) | An optional reverse proxy mentioned in the self-hosting guide. Not bundled. | Apache-2.0 |
| [nginx](https://nginx.org/) | An optional reverse proxy mentioned in the self-hosting guide. Not bundled. | 2-clause BSD |
| [Let's Encrypt](https://letsencrypt.org/) | The certificate authority whose ACME certificates are described in the self-hosting guide. Not bundled. | Public domain / MPL-2.0 (ACME client libraries vary) |
| [certbot](https://github.com/certbot/certbot) | An optional ACME client mentioned in the self-hosting guide. Not bundled. | Apache-2.0 |
| [Cloudflare](https://www.cloudflare.com/) | Mentioned only as an example of a CDN that can supply a country-code header. Not required, not bundled, not affiliated. | Proprietary service |

## Trademarks and naming

"Sumimi" and "数咪咪" are the names of this project. Third-party names —
Umami, DuckDB, Axum, Tokio, Caddy, nginx, Let's Encrypt, Cloudflare — are
referenced nominatively, in accordance with the nominative-fair-use principle,
to identify compatible, comparable or recommended products. No claim of
endorsement, affiliation or sponsorship is made or implied.

Sumimi is an independent open-source project. It is described factually as
*an open-source alternative to Umami that implements the Umami v2 HTTP API*.

## Any future additions

If third-party artwork, photographs, icon sets, web fonts or other media are
ever added to `service/docs/`, that asset must be listed in this file with its source
URL, its author and its licence before it is committed. The design document
(AGENTS.md §3) requires that marketing and documentation assets are either
original or properly licensed, and that the licence of every asset in `service/docs/`
is recorded here.
