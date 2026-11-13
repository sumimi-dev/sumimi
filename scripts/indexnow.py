#!/usr/bin/env python3
"""Tell the search engines that the site changed.

IndexNow (https://www.indexnow.org) is the push protocol Microsoft, Yandex and
others support: hand over the URLs that changed, they come and look. The key
has to be verifiable, so it is also served from the site's own root —
`https://sumimi.dev/<key>.txt` — and that is what makes the push legitimate.

This is the sibling of `gsc-sitemap.py`: that one asks Google to look at the
sitemap, this one pings the engines that listen for pushes. Neither stores
anything on the site; both are one HTTP request.

Usage:

    python3 scripts/indexnow.py                  # every URL in the sitemap
    python3 scripts/indexnow.py /blog/ /docs/    # only these paths
    python3 scripts/indexnow.py --list           # what would be sent

The key is in `service/docs/<key>.txt` — the same file the site serves.
"""

import argparse
import json
import pathlib
import re
import sys
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parent.parent
SITE = "https://sumimi.dev"
ENDPOINT = "https://api.indexnow.org/indexnow"


def find_key():
    """The key, and where it is served from.

    The key lives in `scripts/indexnow.key`; the same string is published as
    `service/docs/<key>.txt`, which is the file the engines fetch to check
    that the push really comes from this site. Both are read here, and a
    mismatch is a loud error rather than a silent wrong key.
    """
    source = ROOT / "scripts" / "indexnow.key"
    if source.exists():
        key = source.read_text(encoding="utf-8").strip()
        served = ROOT / "service" / "docs" / f"{key}.txt"
        if not served.exists():
            print(f"the key file is not published: {served}", file=sys.stderr)
            return None, None
        return key, served.read_text(encoding="utf-8").strip()

    # Fall back to the published directory: the key file is the one .txt whose
    # name is the key.
    docs = ROOT / "service" / "docs"
    for path in sorted(docs.glob("*.txt")):
        stem = path.name.rsplit(".", 1)[0]
        if re.fullmatch(r"[a-zA-Z0-9-]{8,128}", stem) and path.read_text(encoding="utf-8").strip() == stem:
            return stem, stem
    return None, None


def sitemap_urls():
    sm = ROOT / "service" / "docs" / "sitemap.xml"
    if not sm.exists():
        return []
    return re.findall(r"<loc>(.*?)</loc>", sm.read_text(encoding="utf-8"))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("paths", nargs="*", help="only these paths (default: the whole sitemap)")
    ap.add_argument("--list", action="store_true", help="print the URLs and stop")
    args = ap.parse_args()

    key, key_file = find_key()
    if not key:
        print("no IndexNow key found in service/docs/*.txt", file=sys.stderr)
        return 1

    if args.paths:
        urls = [
            p if p.startswith("http") else SITE + (p if p.startswith("/") else "/" + p)
            for p in args.paths
        ]
    else:
        urls = sitemap_urls()

    if not urls:
        print("nothing to send — no sitemap, and no paths given", file=sys.stderr)
        return 1

    if args.list:
        for u in urls:
            print(u)
        return 0

    body = json.dumps({"host": "sumimi.dev", "key": key, "urlList": urls}).encode()
    request = urllib.request.Request(
        f"{ENDPOINT}?key={key}",
        data=body,
        headers={"Content-Type": "application/json; charset=utf-8"},
        method="POST",
    )

    print(f"IndexNow: {len(urls)} url(s), key {key[:8]}…")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            answer = response.read().decode("utf-8", "replace").strip()
            print(f"accepted ({response.status}) {answer}")
            return 0
    except urllib.error.HTTPError as error:
        detail = error.read().decode("utf-8", "replace").strip()
        print(f"rejected ({error.code}) {detail}", file=sys.stderr)
        return 1
    except Exception as error:  # network, DNS, TLS
        print(f"could not reach IndexNow: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
