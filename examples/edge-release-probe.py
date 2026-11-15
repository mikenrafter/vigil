#!/usr/bin/env python3
"""Offline edge release policy: 0 due, 1 not due, 2 unavailable.

Only local objects and refs are read. Missing objects never trigger a fetch.
The caller supplies the same IANA timezone as the reminder.
"""
import argparse
import datetime as dt
import os
from pathlib import PurePosixPath
import subprocess
import sys
from zoneinfo import ZoneInfo


def git(directory, *args):
    # CLAIM(release-offline): both lazy fetch and all transport protocols are
    # disabled [test_edge_release_probe missing_blob regression]. External diff
    # and textconv are disabled at the diff call sites as well.
    return subprocess.run(
        ["git", "-c", "protocol.allow=never", "-c", "core.fsmonitor=false", *args],
        cwd=directory, env={**os.environ, "GIT_NO_LAZY_FETCH": "1", "GIT_TERMINAL_PROMPT": "0"},
        check=True, capture_output=True, text=True, timeout=4,
    ).stdout


def eligible(path):
    parts = PurePosixPath(path).parts
    name = parts[-1]
    return (parts[0] == "lib" and not {"test", "tests"}.intersection(parts[1:-1])
            and not name.endswith(("_test.dart", ".g.dart", ".freezed.dart"))
            and not (path.startswith("lib/l10n/") and name.startswith("app_localizations") and name.endswith(".dart")))


def main(argv=None, now=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dir", default="/home/user/Documents/repos/edge")
    parser.add_argument("--branch", default="refs/remotes/origin/integrate/next")
    parser.add_argument("--tz", required=True)
    parser.add_argument("--min-lines", type=int, default=300)
    parser.add_argument("--min-new-files", type=int, default=2)
    args = parser.parse_args(argv)
    if args.min_lines < 1 or args.min_new_files < 1:
        raise ValueError("thresholds must be positive")
    zone = ZoneInfo(args.tz)
    today = (now or dt.datetime.now(zone)).astimezone(zone).date()
    monday = today - dt.timedelta(days=today.weekday())
    tags = []
    for row in git(args.dir, "for-each-ref", "--sort=-creatordate", "--format=%(refname)%09%(creatordate:unix)", "refs/tags/next-*").splitlines():
        ref, created = row.split("\t")
        tags.append((ref, dt.datetime.fromtimestamp(int(created), zone).date()))
    if tags:
        baseline = tags[0][0]
        label = baseline.removeprefix("refs/tags/")
    else:
        baseline = git(args.dir, "merge-base", "refs/remotes/upstream/main", args.branch).strip()
        label = baseline[:12]
    # A reversed/divergent diff is unavailable, even when a tag is this week.
    git(args.dir, "merge-base", "--is-ancestor", baseline, args.branch)
    for ref, created in tags:
        if monday <= created < monday + dt.timedelta(days=7):
            print(f"Fork tag {ref.removeprefix('refs/tags/')} already created this local week")
            return 1
    # Count the net change since the release, without rename/quoting ambiguity.
    diff_args = ["diff", "--no-ext-diff", "--no-textconv", "--no-renames"]
    rows = git(args.dir, *diff_args, "--numstat", "-z", baseline, args.branch, "--", "lib/")
    lines = 0
    for row in rows.split("\0"):
        if not row:
            continue
        added, removed, path = row.split("\t", 2)
        if eligible(path) and added != "-" and removed != "-":
            lines += int(added) + int(removed)
    new_paths = git(args.dir, *diff_args, "--name-only", "--diff-filter=A", "-z", baseline, args.branch, "--", "lib/")
    files = sum(1 for path in new_paths.split("\0") if path and path.endswith(".dart") and eligible(path))
    rules = []
    if lines >= args.min_lines:
        rules.append("lines rule")
    if files >= args.min_new_files:
        rules.append("new-files rule")
    verdict = " and ".join(rules) if rules else "below both thresholds"
    print(f"{lines} lib lines and {files} new files since {label}; {verdict}")
    return 0 if rules else 1


def run(argv=None, now=None):
    try:
        return main(argv, now)
    except Exception as error:
        # Exit 1 is exclusively a successfully evaluated false predicate.
        print(f"Release check unavailable: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(run())
