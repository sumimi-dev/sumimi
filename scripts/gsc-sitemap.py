#!/usr/bin/env python3
"""Submit the documentation site's sitemap to Google Search Console.

Search Console is the only way to get a page into Google's index on purpose —
a sitemap says "these are the pages", and this says "and here they are, right
now". The API needs a credential: a service account with the Search Console
API enabled and **added as a user of the property**.

The credential comes from the environment:

* `GOOGLE_APPLICATION_CREDENTIALS` — the path of a service account JSON file
  (what the CI workflow writes from its secret), or
* `GSC_SERVICE_ACCOUNT_JSON` — the JSON itself.

Usage:

    python3 scripts/gsc-sitemap.py                     # the site, the sitemap
    python3 scripts/gsc-sitemap.py --list              # what we can see
    python3 scripts/gsc-sitemap.py --site https://… --sitemap https://…

Why it is a script and not a workflow step: the same thing has to be runnable
by hand, from a laptop, on a machine that can reach Google — and the error
messages of the API are worth reading.
"""

import argparse
import json
import os
import sys

try:
    from google.oauth2 import service_account
    from googleapiclient.discovery import build
except ImportError:
    sys.exit(
        "this script needs the Google API client:\n"
        "    pip install google-api-python-client google-auth"
    )

# The site, and the sitemap that describes it.
DEFAULT_SITE = "https://sumimi-dev.github.io/"
DEFAULT_SITEMAP = "https://sumimi-dev.github.io/sitemap.xml"

SCOPES = ["https://www.googleapis.com/auth/webmasters.readonly"]


def credentials():
    """The service account, from wherever it is."""
    path = os.environ.get("GOOGLE_APPLICATION_CREDENTIALS")
    if path:
        return service_account.Credentials.from_service_account_file(path, scopes=SCOPES)

    blob = os.environ.get("GSC_SERVICE_ACCOUNT_JSON")
    if blob:
        return service_account.Credentials.from_service_account_info(
            json.loads(blob), scopes=SCOPES
        )

    sys.exit(
        "no credential: set GOOGLE_APPLICATION_CREDENTIALS to the path of the\n"
        "service account JSON, or GSC_SERVICE_ACCOUNT_JSON to its contents."
    )


def service():
    return build("searchconsole", "v1", credentials=credentials())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--site", default=DEFAULT_SITE, help="the property")
    parser.add_argument("--sitemap", default=DEFAULT_SITEMAP, help="the sitemap")
    parser.add_argument("--list", action="store_true", help="list the properties we can see")
    args = parser.parse_args()

    api = service()

    if args.list:
        for entry in api.sites().list().execute().get("siteEntry", []):
            print(f"{entry['siteUrl']:<45} {entry['permissionLevel']}")
        return

    # What we can see. If the property is not here, the service account is not
    # a user of it — that is the one step that needs a browser.
    sites = api.sites().list().execute().get("siteEntry", [])
    names = [entry["siteUrl"] for entry in sites]
    print(f"the properties this account can see: {names or '(none)'}")

    if args.site not in names:
        sys.exit(
            f"'{args.site}' is not among them.\n"
            "Add the service account as a user of the property in the Search\n"
            "Console, then try again."
        )

    # And the sitemap itself.
    result = api.sitemaps().submit(siteUrl=args.site, feedpath=args.sitemap).execute()
    print(f"submitted {args.sitemap}: {result}")

    # What the Search Console thinks of it.
    listed = api.sitemaps().list(siteUrl=args.site).execute().get("sitemap", [])
    for entry in listed:
        if entry.get("path") == args.sitemap:
            print(
                f"the sitemap is {entry.get('isPending')} / "
                f"{entry.get('isSitemapsIndex')} — "
                f"{entry.get('lastSubmitted')}"
            )


if __name__ == "__main__":
    main()
