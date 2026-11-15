#!/usr/bin/env python3
"""Example only: 0 due, 1 not due, 2 unavailable. Installs nothing.

Published fork releases are read live from GitHub; integrate/next and tags are
local refs. Fetch those before relying on this example. "Substantial" defaults
to 100 added/deleted lib/ lines, an operator policy rather than a release rule.
"""
import argparse
import datetime as dt
import json
import subprocess
import sys
from zoneinfo import ZoneInfo


def command(*argv, cwd=None):
    return subprocess.run(argv, cwd=cwd, check=True, capture_output=True, text=True, timeout=8).stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dir", default="/home/user/Documents/repos/edge")
    parser.add_argument("--repo", default="mikenrafter/edge")
    parser.add_argument("--branch", default="integrate/next")
    parser.add_argument("--tz", default="America/Denver")
    parser.add_argument("--min-lines", type=int, default=100)
    args = parser.parse_args()
    zone = ZoneInfo(args.tz)
    today = dt.datetime.now(zone).date()
    monday = today - dt.timedelta(days=today.weekday())
    releases = json.loads(command("gh", "release", "list", "--repo", args.repo, "--exclude-drafts", "--limit", "100", "--json", "tagName,publishedAt"))
    forks = [r for r in releases if r["tagName"].startswith("next-")]
    forks.sort(key=lambda r: r["publishedAt"], reverse=True)
    if forks:
        latest = forks[0]
        published = dt.datetime.fromisoformat(latest["publishedAt"].replace("Z", "+00:00")).astimezone(zone).date()
        if published >= monday:
            print(f"Fork release {latest['tagName']} already published this week")
            return 1
        baseline = command("git", "rev-parse", "--verify", f"refs/tags/{latest['tagName']}^{{commit}}", cwd=args.dir)
    else:
        # First fork release: count changes since its shared upstream base.
        baseline = command("git", "merge-base", "refs/remotes/upstream/main", args.branch, cwd=args.dir)
    # Inspect only changes that survive on the branch, not commit-message volume.
    diff = command("git", "diff", "--numstat", baseline, args.branch, "--", "lib/", cwd=args.dir)
    lines = 0
    files = 0
    for row in diff.splitlines():
        added, removed, _ = row.split("\t", 2)
        files += 1
        if added != "-" and removed != "-":
            lines += int(added) + int(removed)
    print(f"{lines} changed lib/ lines across {files} files since {baseline[:12]} on {args.branch}; threshold {args.min_lines}")
    return 0 if lines >= args.min_lines else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"Release check unavailable: {error}", file=sys.stderr)
        sys.exit(2)
