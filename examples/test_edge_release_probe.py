"""Owner policy regression tests; real local git repositories, no network."""
import contextlib
import datetime as dt
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("release_probe", Path(__file__).with_name("edge-release-probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)
NOW = dt.datetime(2026, 11, 18, 18, tzinfo=dt.timezone.utc)
BRANCH = "refs/remotes/origin/integrate/next"


class ReleaseProbeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.write("lib/base.dart", "base\n")
        self.commit("base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/upstream/main", self.base)
        self.git("update-ref", "refs/heads/integrate/next", self.base)
        self.git("update-ref", BRANCH, self.base)

    def git(self, *args, env=None):
        return subprocess.check_output(["git", *args], cwd=self.repo, text=True,
                                       env={**os.environ, "GIT_NO_LAZY_FETCH": "1", **(env or {})}).strip()

    def write(self, path, content):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    def commit(self, message):
        self.git("add", ".")
        self.git("commit", "-qm", message, env={"GIT_AUTHOR_DATE": "2026-11-01T12:00:00Z", "GIT_COMMITTER_DATE": "2026-11-01T12:00:00Z"})
        self.git("update-ref", BRANCH, self.git("rev-parse", "HEAD"))

    def tag(self, name="next-2026.11.04", date="2026-11-04T12:00:00Z", target=None):
        self.git("tag", "-a", name, "-m", "release", target or self.base,
                 env={"GIT_COMMITTER_DATE": date})

    def check(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = probe.run(["--dir", str(self.repo), "--tz", "America/Denver", *args], now=NOW)
        return code, out.getvalue(), err.getvalue()

    def test_lagging_local_branch_uses_remote_and_local_tag(self):
        # WHY: C1: worktree pushes update origin/integrate/next, not the local branch.
        self.tag()
        self.write("lib/base.dart", "base\n" + "line\n" * 300)
        self.commit("work")
        code, out, err = self.check()
        self.assertEqual((code, err), (0, ""))
        self.assertIn("300 lib lines", out)
        self.assertIn("next-2026.11.04", out)
        self.assertIn("lines rule", out)
        self.assertEqual(self.git("rev-parse", "integrate/next"), self.base)

    def test_no_tag_uses_upstream_merge_base(self):
        # WHY: the first release must work without invented release metadata.
        self.write("lib/one.dart", "one\n")
        self.write("lib/two.dart", "two\n")
        self.commit("two files")
        code, out, err = self.check()
        self.assertEqual((code, err), (0, ""))
        self.assertIn("2 lib lines and 2 new files", out)
        self.assertIn(self.base[:12], out)
        self.assertIn("new-files rule", out)

    def test_each_threshold_alone_and_exact_boundaries(self):
        # WHY: OR, not AND; inclusive 300 lines / 2 new files, flags override both.
        self.write("lib/base.dart", "base\n" + "line\n" * 299)
        self.commit("299 lines")
        self.assertEqual(self.check()[0], 1)
        self.assertEqual(self.check("--min-lines", "299")[0], 0)
        self.write("lib/base.dart", "base\n" + "line\n" * 300)
        self.commit("300 lines")
        self.assertEqual(self.check()[0], 0)
        self.assertEqual(self.check("--min-lines", "301")[0], 1)
        self.write("lib/a.dart", "a\n")
        self.write("lib/b.dart", "b\n")
        self.commit("new files")
        self.assertEqual(self.check("--min-lines", "99999")[0], 0)
        self.assertEqual(self.check("--min-lines", "99999", "--min-new-files", "3")[0], 1)

    def test_added_and_deleted_lines_both_count(self):
        # WHY: the line rule is added + deleted, not only added or commit volume.
        self.write("lib/base.dart", "old\n" * 150)
        self.commit("old contents")
        self.git("update-ref", "refs/remotes/upstream/main", self.git("rev-parse", "HEAD"))
        self.write("lib/base.dart", "new\n" * 150)
        self.commit("replace contents")
        code, out, err = self.check()
        self.assertEqual((code, err), (0, ""))
        self.assertIn("300 lib lines and 0 new files", out)

    def test_tag_this_local_week_suppresses_and_uses_creatordate(self):
        # WHY: Monday in Denver starts at 07:00Z; annotated tag creation time
        # matters, even when the tagged commit and a lexically later tag are old.
        self.tag("next-zzz", "2026-11-01T12:00:00Z")
        self.write("lib/base.dart", "base\n" + "line\n" * 300)
        self.commit("work")
        self.tag("next-aaa", "2026-11-16T06:59:59Z")  # Sunday Denver
        self.assertEqual(self.check()[0], 0)
        self.tag("next-bbb", "2026-11-16T07:00:00Z")  # Monday Denver
        code, out, err = self.check()
        self.assertEqual((code, err), (1, ""))
        self.assertIn("next-bbb", out)
        self.assertIn("this local week", out)

    def test_generated_and_test_files_are_excluded(self):
        # WHY: generated churn and tests must not trigger either release rule.
        for path in ["lib/l10n/app_localizations.dart", "lib/l10n/app_localizations_en.dart",
                     "lib/model.g.dart", "lib/model.freezed.dart", "lib/unit_test.dart",
                     "lib/test/helper.dart", "lib/tests/helper.dart", "test/outside.dart"]:
            self.write(path, "generated\n" * 400)
        self.commit("generated and tests")
        code, out, err = self.check()
        self.assertEqual((code, err), (1, ""))
        self.assertIn("0 lib lines and 0 new files", out)

    def test_nonancestor_tag_is_unavailable(self):
        # WHY: never count the reverse diff from an unrelated/later release.
        self.write("lib/a.dart", "a\n")
        self.commit("ahead")
        self.tag(target=self.git("rev-parse", "HEAD"))
        self.git("update-ref", BRANCH, self.base)
        self.assertEqual(self.check()[0], 2)

    def test_missing_blob_is_unavailable_and_network_is_disabled(self):
        # WHY: a blob:none clone must abstain, never lazily fetch or count a
        # missing object as zero. A fake remote helper records any network attempt.
        self.write("lib/base.dart", "base\n" + "line\n" * 300)
        self.commit("work")
        blob = self.git("rev-parse", "HEAD:lib/base.dart")
        self.git("config", "remote.origin.promisor", "true")
        self.git("config", "remote.origin.partialclonefilter", "blob:none")
        self.git("config", "remote.origin.url", "trap::must-not-fetch")
        helper = self.repo / "bin/git-remote-trap"
        helper.parent.mkdir()
        helper.write_text("#!/bin/sh\necho network > \"$NETWORK_MARKER\"\nexit 99\n")
        helper.chmod(0o755)
        (self.repo / ".git/objects" / blob[:2] / blob[2:]).unlink()
        marker = self.repo / "network-attempt"
        with patch.dict(os.environ, {"PATH": str(helper.parent) + os.pathsep + os.environ["PATH"], "NETWORK_MARKER": str(marker)}):
            code, out, err = self.check()
        self.assertEqual(code, 2, (out, err))
        self.assertTrue(err.startswith("Release check unavailable:"), err)
        self.assertFalse(marker.exists(), "probe tried a lazy network fetch")

    def test_unexpected_exceptions_are_unavailable(self):
        # WHY: C2: bugs such as TypeError must not silently mean not due.
        with patch.object(probe, "main", side_effect=TypeError("broken")):
            self.assertEqual(self.check()[0], 2)


if __name__ == "__main__":
    unittest.main()
