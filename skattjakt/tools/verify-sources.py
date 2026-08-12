#!/usr/bin/env python3
"""Retrieves every source the rule set cites and checks it says what the rules assume.

The problem this exists for
===========================

The rule set was drafted by a language model. Until now, the only thing standing
between those rules and a customer was a line in a document saying a qualified
person had not yet reviewed them — and, eventually, that person's signature.

A signature is a weak guarantee. It is unfalsifiable, it does not survive the
law changing, and nobody can check it afterwards without repeating the whole
review. A citation to primary law is a stronger guarantee for exactly the
opposite reasons: it names something anybody can go and read, and a machine can
check that it still says what was claimed.

So this program does the checking. For every source in the registry it:

  1. fetches the document from the authority that publishes it;
  2. checks the document is the one cited — the SFS number appears in it;
  3. checks the cited locator (`30 kap. 5 §`) exists in it;
  4. checks every string in `must_contain` appears — these are the operative
     words and figures the rule depends on, so "25 procent" missing from
     30 kap. 5 § means the rate in the rule set is wrong or the law has moved;
  5. records the retrieval: a timestamp and a SHA-256 of the text it read.

What it deliberately cannot do
==============================

**It cannot mark anything verified without having fetched it.** The retrieval
state, the timestamp and the hash are written only by this program, and
`rules/ruleset_test.rs` fails the build if a source claims `verified` without a
hash — so the state cannot be granted by editing a file, by me, or by anyone
else with an opinion.

And it does not judge whether a rule *applies* its source correctly. That a
paragraph says 25 percent does not establish that this rule computes the right
base to apply it to. That question needs a person, and it is the one thing here
a person is genuinely better at than a fetch.

Usage:
    tools/verify-sources.py [--write] [rules/se-ruleset.json]

    --write   update the retrieval state in the rule set. Without it, the
              program reports and changes nothing.

Exit codes: 0 all sources verified; 1 a source contradicted the rule set;
            2 nothing could be retrieved.
"""

import argparse
import collections
import hashlib
import json
import re
import sys
import urllib.error
import urllib.request
from datetime import datetime, timezone

TIMEOUT = 30
AGENT = "skattjakt-source-verifier/1.0 (+source verification for a tax analysis tool)"


def fetch(url):
    """Returns the page as text, or raises with a reason worth reading."""
    request = urllib.request.Request(url, headers={"User-Agent": AGENT})
    with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
        raw = response.read()
    charset = response.headers.get_content_charset() or "utf-8"
    return raw.decode(charset, errors="replace")


def strip_markup(html):
    text = re.sub(r"(?is)<(script|style)[^>]*>.*?</\1>", " ", html)
    text = re.sub(r"(?s)<[^>]+>", " ", text)
    text = (text.replace("&nbsp;", " ").replace("&auml;", "ä").replace("&ouml;", "ö")
                .replace("&aring;", "å").replace("&Auml;", "Ä").replace("&Ouml;", "Ö")
                .replace("&Aring;", "Å").replace("&amp;", "&").replace("&sect;", "§"))
    return re.sub(r"\s+", " ", text)


def normalise(text):
    """Folds the differences that are not differences.

    Statute is published with non-breaking spaces inside figures, with `§§` for
    a range, and with either a comma or a period in a decimal. Comparing raw
    strings would report a mismatch for `20,6` against `20,6` differing only by
    the space before the percent sign.
    """
    text = text.replace(" ", " ").replace("‑", "-").replace("–", "-")
    return re.sub(r"\s+", " ", text).lower()


def locator_pattern(locator):
    """A regex for `30 kap. 5 §`, tolerant of how the text happens to set it.

    The published text writes a paragraph as `5 §` and a range as `5–6 §§`, and
    a chapter heading may be `30 kap.` or `30 kap`. Matching the literal string
    would fail on formatting rather than on substance.
    """
    chapter = re.search(r"(\d+)\s*kap", locator)
    paragraph = re.search(r"(\d+)\s*§", locator)
    parts = []
    if chapter:
        parts.append(rf"{chapter.group(1)}\s*kap\.?")
    if paragraph:
        parts.append(rf"{paragraph.group(1)}\s*§")
    if not parts:
        return None
    return re.compile(r".{0,400}?".join(parts), re.S)


def verify(key, source):
    """Checks one source. Returns (state, note)."""
    url = source.get("machine_url") or source.get("url")
    if not url:
        return "unretrieved", "no url"

    try:
        html = fetch(url)
    except urllib.error.HTTPError as error:
        return "unreachable", f"HTTP {error.code} from {url}"
    except Exception as error:  # noqa: BLE001 — the reason is what matters
        return "unreachable", f"{type(error).__name__}: {error}"

    text = strip_markup(html)
    folded = normalise(text)
    digest = hashlib.sha256(text.encode("utf-8")).hexdigest()

    document = source.get("document", "")
    if source.get("collection") == "SFS" and document:
        if normalise(document) not in folded:
            return "mismatch", f"the retrieved page does not mention SFS {document}"

    pattern = locator_pattern(source.get("locator", ""))
    if pattern and not pattern.search(text):
        return "mismatch", f"could not find {source['locator']} in the retrieved text"

    absent = [needle for needle in source.get("must_contain", [])
              if normalise(needle) not in folded]
    if absent:
        return "mismatch", f"the source does not contain: {', '.join(absent)}"

    return "verified", digest


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("ruleset", nargs="?", default="rules/se-ruleset.json")
    parser.add_argument("--write", action="store_true")
    arguments = parser.parse_args()

    data = json.load(open(arguments.ruleset), object_pairs_hook=collections.OrderedDict)
    sources = data.get("sources", {})
    if not sources:
        print("the rule set has no source registry", file=sys.stderr)
        return 2

    now = datetime.now(timezone.utc).replace(microsecond=0).isoformat()
    verified = mismatched = unreachable = 0

    for key, source in sources.items():
        state, note = verify(key, source)
        retrieval = source["retrieval"]

        if state == "verified":
            verified += 1
            print(f"  ok        {key:<22} {source['locator']}")
            if arguments.write:
                retrieval.update({"state": "verified", "at": now,
                                  "sha256": note, "note": None})
        elif state == "mismatch":
            mismatched += 1
            print(f"  MISMATCH  {key:<22} {note}")
            if arguments.write:
                retrieval.update({"state": "mismatch", "at": now,
                                  "sha256": None, "note": note})
        else:
            unreachable += 1
            print(f"  unreached {key:<22} {note}")
            if arguments.write:
                # Deliberately does not clear an earlier successful retrieval:
                # a network failure today is not evidence about the law, and
                # discarding yesterday's verified hash because a proxy said no
                # would make the record less true rather than more.
                if retrieval.get("state") != "verified":
                    retrieval.update({"state": "unretrieved", "at": None,
                                      "sha256": None, "note": note})

    if arguments.write:
        json.dump(data, open(arguments.ruleset, "w"), ensure_ascii=False, indent=2)
        print(f"\nwrote retrieval state to {arguments.ruleset}")

    total = len(sources)
    print(f"\n{verified} verified, {mismatched} contradicted, "
          f"{unreachable} unreachable, of {total} sources")

    if mismatched:
        print("\nA source contradicted the rule set. Either the law has changed or the "
              "rule set was wrong when it was written; both need a person before "
              "anything is shipped.")
        return 1
    if verified == 0:
        print("\nNothing could be retrieved, so nothing is verified. The rules keep "
              "the status ceiling that unverified sources earn them.")
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
