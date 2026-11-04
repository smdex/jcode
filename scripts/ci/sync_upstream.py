#!/usr/bin/env python3
"""Rebase the fork, tag new upstream stable versions, mirror, and release sources."""

import json
import os
import re
import subprocess
import tempfile
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

STABLE = re.compile(r"v(\d+)\.(\d+)\.(\d+)$")
FORK = re.compile(r"(\d+)\.(\d+)\.(\d+)-smdex\.(\d+)$")


def git(repo, *args, check=True):
    result = subprocess.run(
        ["git", "-c", "gc.auto=0", "-c", "maintenance.auto=false", *args],
        cwd=repo,
        text=True,
        capture_output=True,
        check=False,
    )
    if check and result.returncode:
        raise RuntimeError(f"git {args[0]} failed:\n{result.stderr}")
    return result


def output(repo, *args):
    return git(repo, *args).stdout.strip()


def version(repo, revision="HEAD"):
    data = tomllib.loads(output(repo, "show", f"{revision}:Cargo.toml"))
    return data["package"]["version"]


def set_version(repo, value):
    patterns = {
        "Cargo.toml": r'(?ms)(^\[package\]\n(?:(?!^\[).)*?^version = ")[^"]+("$)',
        "Cargo.lock": r'(?m)(^name = "jcode"\nversion = ")[^"]+("$)',
    }
    for name, pattern in patterns.items():
        path = Path(repo, name)
        text, count = re.subn(pattern, lambda m: m[1] + value + m[2], path.read_text())
        if count != 1:
            raise RuntimeError(f"Expected one jcode version in {name}, got {count}")
        path.write_text(text)


def resolve_version_conflict(text, name, fork_version):
    """Only resolve a one-line version conflict inside the root jcode package."""
    pattern = re.compile(
        r"(?m)^<<<<<<<[^\n]*\n(.*?)^=======\n(.*?)^>>>>>>>[^\n]*\n", re.DOTALL
    )
    matches = list(pattern.finditer(text))
    if not matches:
        raise ValueError("Not a version conflict")
    for match in matches:
        for side in (match[1], match[2]):
            if not re.fullmatch(r'version = "\d+\.\d+\.\d+(?:-smdex\.\d+)?"\n', side):
                raise ValueError("Conflict includes more than a version line")
        prefix = text[: match.start()]
        headers = re.findall(r"(?m)^\[.*\]$", prefix)
        if name == "Cargo.toml":
            if not headers or headers[-1] != "[package]":
                raise ValueError("Not the root package")
        elif name == "Cargo.lock":
            package = prefix.rsplit("[[package]]", 1)[-1]
            if not re.search(r'(?m)^name = "jcode"$', package):
                raise ValueError("Not the root lockfile package")
        else:
            raise ValueError("Not a version file")
    resolved = pattern.sub(lambda _: f'version = "{fork_version}"\n', text)
    tomllib.loads(resolved)
    return resolved


def rebase(repo, fork_version):
    result = git(repo, "rebase", "upstream/master", check=False)
    while result.returncode:
        paths = output(repo, "diff", "--name-only", "--diff-filter=U").splitlines()
        try:
            if not paths or not set(paths) <= {"Cargo.toml", "Cargo.lock"}:
                raise ValueError("Code conflict needs manual resolution")
            resolved = {
                name: resolve_version_conflict(
                    Path(repo, name).read_text(), name, fork_version
                )
                for name in paths
            }
            for name, text in resolved.items():
                Path(repo, name).write_text(text)
            git(repo, "add", "--", *paths)
            result = git(
                repo, "-c", "core.editor=true", "rebase", "--continue", check=False
            )
        except ValueError as error:
            git(repo, "rebase", "--abort")
            raise RuntimeError(
                f"Rebase aborted; remotes unchanged: {error}\n{result.stderr}"
            ) from error


def latest_stable(repo):
    candidates = []
    for ref in output(
        repo, "for-each-ref", "--format=%(refname)", "refs/upstream-tags/"
    ).splitlines():
        tag = ref.removeprefix("refs/upstream-tags/")
        match = STABLE.fullmatch(tag)
        if match:
            candidates.append((tuple(map(int, match.groups())), tag, ref))
    for core, tag, ref in sorted(candidates, reverse=True):
        if (
            git(
                repo,
                "merge-base",
                "--is-ancestor",
                f"{ref}^{{commit}}",
                "upstream/master",
                check=False,
            ).returncode
            == 0
        ):
            return core, tag, output(repo, "rev-parse", f"{ref}^{{commit}}")
    return None


def next_version(repo, current, stable):
    match = FORK.fullmatch(current)
    if not match:
        raise RuntimeError(f"Expected a fork version, got {current!r}")
    if not stable or stable[0] < tuple(map(int, match.groups()[:3])):
        return current
    annotations = output(repo, "for-each-ref", "--format=%(contents)", "refs/tags/")
    if f"Upstream-release: {stable[1]}" in annotations.splitlines():
        return current
    core = ".".join(map(str, stable[0]))
    revisions = [0]
    for tag in output(repo, "tag", "--list", f"v{core}-smdex.*").splitlines():
        candidate = FORK.fullmatch(tag.removeprefix("v"))
        if candidate:
            revisions.append(int(candidate[4]))
    if stable[0] == tuple(map(int, match.groups()[:3])):
        revisions.append(int(match[4]))
    return f"{core}-smdex.{max(revisions) + 1}"


def sync(repo, github, upstream, mirror, run_id, release):
    git(repo, "clone", "--no-tags", "--branch", "main", github, ".")
    git(repo, "config", "user.name", os.environ["GIT_AUTHOR_NAME"])
    git(repo, "config", "user.email", os.environ["GIT_AUTHOR_EMAIL"])
    git(repo, "config", "merge.conflictStyle", "merge")
    git(repo, "remote", "add", "upstream", upstream)
    git(repo, "remote", "add", "mirror", mirror)
    git(
        repo,
        "fetch",
        "--no-tags",
        "upstream",
        "+refs/heads/master:refs/remotes/upstream/master",
        "+refs/tags/v*:refs/upstream-tags/v*",
    )
    git(repo, "fetch", "--no-tags", "origin", "refs/tags/*:refs/tags/*")
    original = output(repo, "rev-parse", "HEAD")
    current = version(repo)
    stable = latest_stable(repo)
    target = next_version(repo, current, stable)
    rebase(repo, current)
    set_version(repo, target)
    if output(repo, "status", "--porcelain"):
        git(repo, "add", "Cargo.toml", "Cargo.lock")
        git(repo, "commit", "-m", f"release: set fork version {target}")
    lock = tomllib.loads(Path(repo, "Cargo.lock").read_text())
    if [p["version"] for p in lock["package"] if p["name"] == "jcode"] != [target]:
        raise RuntimeError("Cargo.toml and Cargo.lock versions disagree")
    git(repo, "diff", "--check", "upstream/master...HEAD")
    tag = f"v{target}"
    if target != current:
        if stable is None:
            raise RuntimeError("Cannot release without an upstream stable tag")
        git(
            repo,
            "tag",
            "-a",
            tag,
            "-m",
            (
                f"Jcode {target}\n\nUpstream-release: {stable[1]}\n"
                f"Upstream-release-commit: {stable[2]}\n"
                f"Upstream-master: {output(repo, 'rev-parse', 'upstream/master')}"
            ),
        )
    has_tag = (
        git(repo, "show-ref", "--verify", f"refs/tags/{tag}", check=False).returncode
        == 0
    )
    head = output(repo, "rev-parse", "HEAD")
    if head != original:
        git(
            repo, "push", "origin", f"{original}:refs/heads/sync-backup/gitlab-{run_id}"
        )
    refs = ["HEAD:refs/heads/main"] + ([f"refs/tags/{tag}"] if has_tag else [])
    git(
        repo,
        "push",
        "--atomic",
        f"--force-with-lease=refs/heads/main:{original}",
        "origin",
        *refs,
    )
    remote = output(repo, "ls-remote", "mirror", "refs/heads/main")
    expected = remote.split()[0] if remote else ""
    git(
        repo,
        "push",
        "--atomic",
        f"--force-with-lease=refs/heads/main:{expected}",
        "mirror",
        *refs,
    )
    print(f"Synced main: {head}; fork version: {target}")
    if has_tag:
        source = output(repo, "rev-parse", f"refs/tags/{tag}^{{commit}}")
        description = (
            f"Source-only Jcode fork release `{tag}`. No prebuilt binaries.\n\n"
            f"Source commit: `{source}`.\n\n"
            f"[Canonical GitHub tag](https://github.com/smdex/jcode/tree/{tag}) · "
            f"[Upstream](https://github.com/1jehuang/jcode)\n\n"
            "Includes the retained fork customizations. Source archives are available below. "
            "Release creation checks repository consistency, not a full Rust build/test suite."
        )
        release(tag, source, description)
    else:
        print(
            f"No existing {tag}; no release until a new upstream stable version arrives."
        )
    return head, target


def gitlab_release(tag, source, description):
    base = (
        f"{os.environ['CI_API_V4_URL']}/projects/{os.environ['CI_PROJECT_ID']}/releases"
    )
    headers = {"JOB-TOKEN": os.environ["CI_JOB_TOKEN"]}
    url = base + "/" + urllib.parse.quote(tag, safe="")
    try:
        with urllib.request.urlopen(
            urllib.request.Request(url, headers=headers), timeout=30
        ) as response:
            existing = json.load(response)
        if existing["commit"]["id"] != source:
            raise RuntimeError(f"Existing release {tag} targets another commit")
        print(f"Release already exists: {existing['_links']['self']}")
        return
    except urllib.error.HTTPError as error:
        if error.code != 404:
            raise
    payload = json.dumps(
        {"tag_name": tag, "name": tag, "description": description}
    ).encode()
    request = urllib.request.Request(
        base, data=payload, headers={**headers, "Content-Type": "application/json"}
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        created = json.load(response)
    print(f"Created source release: {created['_links']['self']}")


def main():
    if (
        os.environ.get("CI_COMMIT_REF_PROTECTED") != "true"
        or os.environ.get("CI_COMMIT_BRANCH") != "main"
    ):
        raise RuntimeError("Maintenance must run on protected main")
    key = Path(os.environ["GITHUB_SSH_KEY"])
    key.chmod(0o600)
    known_hosts = Path(__file__).with_name("github_known_hosts").resolve()
    import shlex

    os.environ["GIT_SSH_COMMAND"] = (
        f"ssh -i {shlex.quote(str(key))} -o IdentitiesOnly=yes -o BatchMode=yes "
        f"-o StrictHostKeyChecking=yes -o UserKnownHostsFile={shlex.quote(str(known_hosts))}"
    )
    os.environ["GIT_TERMINAL_PROMPT"] = "0"
    os.environ["GIT_ASKPASS"] = str(
        Path(__file__).with_name("gitlab_askpass.sh").resolve()
    )
    with tempfile.TemporaryDirectory(prefix="jcode-sync-") as repo:
        sync(
            repo,
            "git@github.com:smdex/jcode.git",
            "https://github.com/1jehuang/jcode.git",
            os.environ["CI_PROJECT_URL"].replace(
                "https://", "https://gitlab-ci-token@", 1
            )
            + ".git",
            os.environ["CI_PIPELINE_ID"],
            gitlab_release,
        )


if __name__ == "__main__":
    main()
