#!/usr/bin/env python3
"""Write the documentation site's sitemap from the pages that exist.

**Why a generator and not a file somebody edits.** A hand-written sitemap has
two ways to go wrong, and both of them are silent:

* a page that was added is **missing** from it — the page is then only found
  through links, and the sitemap (which is how the site says "these are my
  pages") does not say it;
* the `lastmod` of every page says a date that has nothing to do with when the
  page last **changed** — Google reads a stale date as "this site is not
  maintained" and crawls it less.

So the sitemap is written from the pages themselves: every `index.html` under
`service/docs/` is one entry, and `lastmod` is the date of the commit that
last touched that file — **the truth, not a number somebody remembered**.

Usage:

    python3 scripts/gen-sitemap.py               # write the sitemap
    python3 scripts/gen-sitemap.py --check       # fail if the file is not what
                                                 # this script writes

The `--check` is what the continuous integration runs: the file in the
repository has to be the generated one, so nobody can edit it by hand and
quietly make it wrong again.
"""

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOCS = ROOT / "service" / "docs"
SITEMAP = DOCS / "sitemap.xml"

# The site the pages live on.
SITE = "https://sumimi.dev"

# How often each page changes, and how much it matters. **The home page is
# the one everybody looks at**; the blog posts are read once and then they
# sit; the rest is reference.
SETTINGS = {
    "": ("weekly", "1.0"),
    "docs/": ("weekly", "0.8"),
    "install/": ("weekly", "0.9"),
    "self-host/": ("weekly", "0.9"),
    "tracker/": ("monthly", "0.7"),
    "api/": ("weekly", "0.8"),
    "cli/": ("weekly", "0.8"),
    "memory/": ("weekly", "0.8"),
    "privacy/": ("monthly", "0.7"),
    "faq/": ("monthly", "0.7"),
    "blog/": ("weekly", "0.8"),
    "blog/leaving-umami/": ("monthly", "0.6"),
    "blog/why-duckdb/": ("monthly", "0.6"),
    "blog/how-much-traffic/": ("monthly", "0.6"),
}
DEFAULT_SETTINGS = ("weekly", "0.8")


def pages():
    """The pages of the site: every `index.html`, as a path ending in `/`.

    **The home page comes first** — it is the one everybody looks at — and the
    rest are in alphabetical order, so the file is the same every time.
    """
    out = []
    for index in sorted(DOCS.rglob("index.html")):
        relative = index.parent.relative_to(DOCS)
        out.append(str(relative).replace(os.sep, "/") + "/" if str(relative) != "." else "")
    out.sort()
    if "" in out:
        out.remove("")
        out.insert(0, "")
    return out


def lastmod(page):
    """When the page last changed — the date of the commit that touched it.

    The **file's** timestamp is not it: in a checkout every file carries the
    moment it was written, so a fresh checkout would say "every page changed
    today" ⇒ ⇒ Google would think the whole site is new every time.
    """
    try:
        stamp = subprocess.run(
            ["git", "log", "-1", "--format=%aI", "--", str(DOCS / page / "index.html")],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        stamp = ""
    if not stamp:
        # No history (a shallow checkout, or the file is not committed yet).
        return "1970-01-01"
    return stamp[:10]


def render():
    """The sitemap, as text."""
    lines = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
    ]
    for page in pages():
        changefreq, priority = SETTINGS.get(page, DEFAULT_SETTINGS)
        lines.append("  <url>")
        lines.append(f"    <loc>{SITE}/{page}</loc>")
        lines.append(f"    <lastmod>{lastmod(page)}</lastmod>")
        lines.append(f"    <changefreq>{changefreq}</changefreq>")
        lines.append(f"    <priority>{priority}</priority>")
        lines.append("  </url>")
    lines.append("</urlset>")
    return "\n".join(lines) + "\n"


def workspace_version():
    """The version the workspace is at — the version the release publishes."""
    text = (ROOT / "Cargo.toml").read_text()
    match = re.search(r'^version = "([^"]+)"', text, re.MULTILINE)
    return match.group(1) if match else "0.0.0"


def sync_version():
    """Put the version the release is at into the structured data.

    **The structured data says which version this is** — and a version that
    is two releases old is a wrong fact on the front page ⇒ ⇒ it is written
    from `Cargo.toml`, the one place the version lives.
    """
    version = workspace_version()
    changed = []
    for path in DOCS.rglob("*.html"):
        text = path.read_text()
        fixed = re.sub(r'("softwareVersion":\s*")[^"]+(")', rf"\g<1>{version}\g<2>", text)
        if fixed != text:
            path.write_text(fixed)
            changed.append(str(path.relative_to(ROOT)))
    return version, changed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if the file is not what this script writes (what CI runs)",
    )
    args = parser.parse_args()

    text = render()
    version, changed = sync_version()
    for path in changed:
        print(f"the version in {path} is now {version}")

    if args.check:
        current = SITEMAP.read_text() if SITEMAP.exists() else ""
        if current != text:
            print(
                "the sitemap is not what the generator writes:\n"
                "    python3 scripts/gen-sitemap.py\n"
                "and commit what it writes.",
                file=sys.stderr,
            )
            sys.exit(1)
        print(f"the sitemap is up to date ({len(pages())} pages)")
        return

    SITEMAP.write_text(text)
    print(f"wrote {SITEMAP.relative_to(ROOT)} ({len(pages())} pages)")


if __name__ == "__main__":
    main()
