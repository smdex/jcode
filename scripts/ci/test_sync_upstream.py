"""Run with: python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import sync_upstream as sync


class SyncTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.upstream = self.root / "upstream"
        self.fork = self.root / "fork.git"
        self.mirror = self.root / "mirror.git"
        self.upstream.mkdir()
        self.run_git(self.upstream, "init", "-b", "master")
        self.run_git(self.upstream, "config", "user.name", "Test Contributor")
        self.run_git(
            self.upstream, "config", "user.email", "contributor@example.invalid"
        )
        (self.upstream / "Cargo.toml").write_text(
            '[package]\nname = "jcode"\nversion = "1.0.0"\n'
        )
        (self.upstream / "Cargo.lock").write_text(
            'version = 4\n\n[[package]]\nname = "jcode"\nversion = "1.0.0"\n'
        )
        (self.upstream / "code").write_text("upstream\n")
        self.commit(self.upstream, "upstream release")
        self.run_git(self.upstream, "tag", "v1.0.0")
        self.run_git(self.root, "clone", "--bare", str(self.upstream), str(self.fork))
        self.run_git(self.root, "init", "--bare", str(self.mirror))
        for bare in (self.fork, self.mirror):
            self.run_git(bare, "config", "receive.autogc", "false")
        self.work = self.root / "work"
        self.run_git(self.root, "clone", str(self.fork), str(self.work))
        self.run_git(self.work, "checkout", "-b", "main")
        self.run_git(self.work, "config", "user.name", "Test Contributor")
        self.run_git(self.work, "config", "user.email", "contributor@example.invalid")
        sync.set_version(self.work, "1.0.0-smdex.1")
        (self.work / "custom").write_text("retained fork feature\n")
        self.commit(self.work, "fork customization")
        self.run_git(
            self.work, "tag", "-a", "v1.0.0-smdex.1", "-m", "Upstream-release: v1.0.0"
        )
        self.run_git(self.work, "push", "origin", "main", "--tags")
        self.releases = []
        self.env = patch.dict(
            os.environ,
            {
                "GIT_AUTHOR_NAME": "Test Contributor",
                "GIT_AUTHOR_EMAIL": "contributor@example.invalid",
            },
        )
        self.env.start()
        self.addCleanup(self.env.stop)
        self.count = 0

    def run_git(self, repo, *args):
        return sync.output(repo, *args)

    def commit(self, repo, message):
        self.run_git(repo, "add", ".")
        self.run_git(repo, "commit", "-m", message)

    def advance(self, value="1.1.0", tag=True):
        sync.set_version(self.upstream, value)
        self.commit(self.upstream, "next upstream version")
        if tag:
            self.run_git(self.upstream, "tag", f"v{value}")

    def execute(self, release=None, mirror=None):
        self.count += 1
        repo = self.root / f"job-{self.count}"
        repo.mkdir()
        return sync.sync(
            repo,
            str(self.fork),
            str(self.upstream),
            str(mirror or self.mirror),
            str(self.count),
            release or (lambda *args: self.releases.append(args)),
        )

    def test_new_stable_version_and_idempotent_retry(self):
        self.advance()
        head, value = self.execute()
        self.assertEqual(value, "1.1.0-smdex.1")
        tag = "v" + value
        self.assertEqual(
            self.run_git(self.fork, "rev-parse", f"{tag}^{{commit}}"), head
        )
        self.assertEqual(self.run_git(self.mirror, "rev-parse", "main"), head)
        self.assertEqual(
            self.run_git(self.mirror, "show", "main:custom"), "retained fork feature"
        )
        self.assertIn(
            "Upstream-release: v1.1.0",
            self.run_git(self.fork, "tag", "-l", "--format=%(contents)", tag),
        )
        again, same = self.execute()
        self.assertEqual((again, same), (head, value))
        self.assertEqual(self.releases[0][0], tag)
        self.assertEqual(self.releases[-1][1], head)

    def test_preview_does_not_release_but_syncs_main(self):
        self.advance("1.1.0", tag=False)
        self.run_git(self.upstream, "tag", "v1.1.0-preview.1")
        head, value = self.execute()
        self.assertEqual(value, "1.0.0-smdex.1")
        self.assertEqual(self.run_git(self.mirror, "rev-parse", "main"), head)
        self.assertEqual(self.releases[0][0], "v1.0.0-smdex.1")

    def test_upstream_older_than_fork_never_downgrades(self):
        sync.set_version(self.work, "1.2.0-smdex.6")
        self.commit(self.work, "ahead fork version")
        self.run_git(self.work, "push", "origin", "main")
        _, value = self.execute()
        self.assertEqual(value, "1.2.0-smdex.6")
        self.assertEqual(self.releases, [])

    def test_merged_feature_rebase_keeps_upstream_copy(self):
        (self.upstream / "custom").write_text("retained fork feature\n")
        self.commit(self.upstream, "merge fork feature upstream")
        head, _ = self.execute()
        self.assertEqual(
            self.run_git(self.fork, "show", f"{head}:custom"), "retained fork feature"
        )

    def test_code_conflict_leaves_both_remote_branches_unchanged(self):
        (self.work / "code").write_text("fork change\n")
        self.commit(self.work, "fork edit")
        self.run_git(self.work, "push", "origin", "main")
        before = self.run_git(self.fork, "rev-parse", "main")
        (self.upstream / "code").write_text("upstream change\n")
        self.commit(self.upstream, "upstream edit")
        with self.assertRaisesRegex(RuntimeError, "Rebase aborted"):
            self.execute()
        self.assertEqual(self.run_git(self.fork, "rev-parse", "main"), before)
        self.assertEqual(self.run_git(self.mirror, "for-each-ref"), "")
        self.assertEqual(self.releases, [])

    def test_release_api_failure_is_recoverable(self):
        self.advance()

        def fail(*args):
            raise RuntimeError("API unavailable")

        with self.assertRaisesRegex(RuntimeError, "API unavailable"):
            self.execute(release=fail)
        head = self.run_git(self.fork, "rev-parse", "main")
        again, value = self.execute()
        self.assertEqual(again, head)
        self.assertEqual(value, "1.1.0-smdex.1")
        self.assertEqual(self.releases[0][1], head)

    def test_gitlab_push_failure_is_recoverable(self):
        self.advance()
        with self.assertRaises(RuntimeError):
            self.execute(mirror=self.root / "missing.git")
        head = self.run_git(self.fork, "rev-parse", "main")
        again, _ = self.execute()
        self.assertEqual(again, head)
        self.assertEqual(self.run_git(self.mirror, "rev-parse", "main"), head)

    def test_competing_github_push_is_not_overwritten(self):
        before = self.run_git(self.fork, "rev-parse", "main")
        self.advance()
        # A real push advances main just before the sync's leased publication.
        original_git = sync.git
        advanced = []

        def competing_git(repo, *args, **kwargs):
            if args[:2] == ("push", "--atomic") and "origin" in args and not advanced:
                (self.work / "concurrent").write_text("do not lose\n")
                self.commit(self.work, "concurrent user change")
                self.run_git(self.work, "push", "origin", "main")
                advanced.append(self.run_git(self.fork, "rev-parse", "main"))
            return original_git(repo, *args, **kwargs)

        with (
            patch.object(sync, "git", side_effect=competing_git),
            self.assertRaises(RuntimeError),
        ):
            self.execute()
        self.assertNotEqual(advanced[0], before)
        self.assertEqual(self.run_git(self.fork, "rev-parse", "main"), advanced[0])
        self.assertEqual(self.run_git(self.fork, "tag", "--list", "v1.1.0-smdex.1"), "")

    def test_legacy_tag_gets_next_suffix_without_moving_existing_tag(self):
        self.run_git(
            self.work, "tag", "-f", "-a", "v1.0.0-smdex.1", "-m", "legacy release"
        )
        self.run_git(self.work, "push", "--force", "origin", "refs/tags/v1.0.0-smdex.1")
        original = self.run_git(self.fork, "rev-parse", "refs/tags/v1.0.0-smdex.1")
        head, value = self.execute()
        self.assertEqual(value, "1.0.0-smdex.2")
        self.assertEqual(
            self.run_git(self.fork, "rev-parse", "refs/tags/v1.0.0-smdex.1"), original
        )
        self.assertEqual(self.execute(), (head, value))

    def test_askpass_only_supplies_token_to_expected_gitlab_host(self):
        script = Path(sync.__file__).with_name("gitlab_askpass.sh")
        env = {
            **os.environ,
            "CI_JOB_TOKEN": "test-secret",
            "CI_SERVER_HOST": "git.example.invalid",
        }
        for host, expected in [("git.example.invalid", 0), ("github.com", 1)]:
            result = subprocess.run(
                ["sh", str(script), f"Password for 'https://gitlab-ci-token@{host}': "],
                env=env,
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, expected)
            self.assertEqual(result.stdout, "test-secret\n" if expected == 0 else "")

    def test_version_resolver_rejects_dependency_and_code_conflicts(self):
        for text, name in [
            (
                '[[package]]\nname = "other"\n<<<<<<< HEAD\nversion = "1.1.0"\n=======\nversion = "1.0.0"\n>>>>>>> fork\n',
                "Cargo.lock",
            ),
            (
                '[package]\nname = "jcode"\n<<<<<<< HEAD\nversion = "1.1.0"\ncode = "upstream"\n=======\nversion = "1.0.0"\ncode = "fork"\n>>>>>>> fork\n',
                "Cargo.toml",
            ),
        ]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                sync.resolve_version_conflict(text, name, "1.0.0-smdex.1")


if __name__ == "__main__":
    unittest.main()
