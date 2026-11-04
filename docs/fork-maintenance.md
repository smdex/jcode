# Fork maintenance

Canonical source is `https://github.com/smdex/jcode`, branch `main`.
Upstream is `https://github.com/1jehuang/jcode`, branch `master`.
The private GitLab mirror and source releases are at
`https://git.fila.lab/smaximov/jcode`.

## Automatic sync and releases

GitLab CI runs hourly at minute 23 UTC. Maintainers can also run the pipeline
manually on protected `main`. GitLab is the only scheduler; the old GitHub sync
workflow has been removed to avoid competing rebases.

`scripts/ci/sync_upstream.py` fetches the current GitHub branch and upstream
master into an isolated clone, then rebases the fork. It creates a recovery
branch `sync-backup/gitlab-<pipeline-id>` before updating GitHub. Publication uses
an exact force-with-lease and an atomic branch/tag push on each host. Normal
source conflicts abort without changing either remote. Only conflicts consisting
exclusively of root `jcode` package version lines in Cargo.toml/Cargo.lock are
resolved automatically; dependency and code conflicts require human review.

Upstream stable tags matching `vMAJOR.MINOR.PATCH` and reachable from upstream
master trigger fork releases. Preview tags do not. The fork integrates current
upstream **master**, which can be ahead of the triggering stable tag. A new
upstream version gets `MAJOR.MINOR.PATCH-smdex.1`; an already-used version core
gets the next available fork suffix. Both Cargo.toml and Cargo.lock are updated.
Existing fork versions are never downgraded: while the fork is
`0.99.2-smdex.6` and upstream stable is `0.99.1`, sync continues without a new
release. An upstream `v0.99.2` release would produce `v0.99.2-smdex.7`.

New annotated fork tags record the triggering upstream release, its commit, and
the integrated upstream master commit. That provenance prevents duplicate tags
on retries. Existing tags are never moved. The current fork tag is mirrored to
GitLab even if it predates automation, and its GitLab source release is created
if missing. **GitHub receives tags, not GitHub releases.** GitLab releases
provide source archives and commit provenance; no binary assets are built.
The workflow runs its Python regression tests and Git consistency checks, not
the full Rust build/test suite. Review other CI before adopting updates in Nix.

Two hosts cannot be updated transactionally. If GitHub succeeds but GitLab or
its release API fails, rerunning reconciles the mirror/release without another
version bump. GitLab is a managed mirror: make source edits on GitHub, not on
the mirror. The GitLab resource group serializes maintenance pipelines; leased
GitHub pushes also reject concurrent changes outside GitLab.

## Provisioned GitLab settings

- Project `smaximov/jcode` (ID 311), private, default branch `main`.
- Runner `usa-docker-nix` (ID 103), job image `alpine:3.22`.
- Protected `main`: Maintainer pushes, force pushes allowed for leased rebases.
- Project-local CI job-token Git pushes enabled. The short-lived `CI_JOB_TOKEN`
  pushes the GitLab mirror and authenticates to the GitLab Releases API. Job-token
  pushes do not trigger recursive pipelines.
- `GITHUB_SSH_KEY`: protected file variable scoped to environment
  `fork-maintenance`, containing a write-enabled deploy key for **only**
  `smdex/jcode`. It is not a personal access token. The multiline key is not
  maskable; do not enable debug tracing or print its contents.
- SSH host keys are pinned in `scripts/ci/github_known_hosts`, obtained from
  GitHub's authenticated HTTPS metadata API. Update them after a verified GitHub
  host-key rotation, never disable host-key checking.
- Pipelines accept schedule, web, and API invocation on protected main only;
  merge-request, fork, tag, and ordinary push pipelines are excluded. The
  environment scope is defense in depth, not a protected-environment policy.

If moving projects/runners, update the runner assignment and job tag, install a
new repository-scoped GitHub deploy key as the protected file variable, enable
project job-token pushes, protect main, and recreate the hourly schedule. No
long-lived GitLab access token is required.

## Local recovery after an automatic rebase

Preserve local work before updating to the rewritten remote history. In this
colocated Jujutsu checkout:

```sh
jj git fetch --remote origin
jj bookmark create backup/local-before-sync -r @-
jj new main@origin
```

For a plain Git clone, first commit or stash all changes, then:

```sh
git fetch origin
git branch backup/local-before-sync
git switch main
git reset --hard origin/main
```

Only run the reset after preserving local work. Recovery branches are available
on GitHub; the original published tags also retain their history.

Regression checks:

```sh
python3 -m unittest discover -s scripts/ci -p 'test_*.py' -v
```
