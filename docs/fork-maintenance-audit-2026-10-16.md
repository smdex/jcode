# Fork-only maintenance audit — smdex/jcode vs 1jehuang/jcode

Date: 2026-10-16. Baseline: fork HEAD `4a60b9652` (= `origin/main`, tag v0.98.2-smdex.1)
vs upstream `upstream/master` = `95bf0e557` (verified fresh via ls-remote on 2026-10-16).

Scope: the fork-only maintenance/packaging/sync commits of the 22 commits in
`upstream/master..HEAD` (app features — ACP sessions, skills reload, external
editor, theme/OSC 52, composer autoscroll — are audited separately).

## Inventory and classification

| Commit | Content | Class |
| --- | --- | --- |
| `534a6f582` | `.github/workflows/sync-upstream.yml`: daily 04:23 UTC rebase of `main` onto `upstream/master`, `--rebase-merges`, backup branch `sync-backup/<run-id>-<attempt>`, abort on conflict, `--force-with-lease` push | fork-only infra |
| `c8d24417c` | `docs/fork-maintenance.md`: sync credentials (`UPSTREAM_SYNC_TOKEN`), recovery, caveats | fork-only docs |
| `35f0c6d2a` | `crates/jcode-build-meta/build.rs`: `parse_semver` tolerates prerelease/build metadata + unit tests; fork `ci.yml` Windows job pins `JCODE_BUILD_SEMVER` from Cargo.toml; version bump smdex.7 | partly upstream-worthy |
| `eb4b71375` | build.rs displays full build semver (`v0.95.1-smdex.8 (hash)`) instead of truncating to `major.minor.patch`; bump smdex.8 | fork-only (optional upstream) |
| `7850606d7` | "rebase latest upstream and bump to 0.95.1-smdex.9" — **also** removes the synthetic `medium`/`efforts[0]` default reasoning-effort from ACP `session_config_options` (`src/cli/acp/session_configuration.rs`) | mislabeled behavior change |
| `f4459da4e` | release bump + rustfmt-only fixes (`crates/jcode-tui/src/tui/app.rs` mod order, `ui_input.rs` line wrap) | mislabeled lint fix |
| `a9cabb825` | release bump + clippy fix in `src/cli/acp.rs` (`if let Err … return Err` → `send_result?`) | mislabeled lint fix |
| `4a60b9652`, `d5ea6c578`, `bcf7f51cf`, `0b0b82140`, `4a553cd70`, `a689e66ec`, `4e3bbf84f` | pure Cargo.toml/Cargo.lock version bumps for the external Nix packaging of the fork (repo `flake.nix` is upstream's dev shell, byte-identical — `git diff upstream/master...HEAD -- flake.nix flake.lock` is empty) | fork-only churn |

## Audit findings

1. **The daily rebase does not alter upstream commits.**
   `git rev-list --left-right --count upstream/master...HEAD` = `0 22` and
   `git merge-base upstream/master HEAD` = `95bf0e557` = upstream tip: all upstream
   SHAs are preserved; only the 22 fork commits are replayed (their SHAs are
   rewritten on every successful sync — visible as duplicate subjects across the 8
   `origin/sync-backup/*` branches).

2. **The sync workflow is only partially reliable.** Run history
   (`gh run list --repo smdex/jcode --workflow sync-upstream.yml`): 8 runs,
   4 success / 4 failure. The 2026-10-14 run `38543358954` failed with
   `CONFLICT (content): Merge conflict in crates/jcode-tui/src/tui/app/local.rs`;
   the workflow aborts safely (main unchanged, backup recorded) but every conflict
   needs a manual rebase+push (that is what `7850606d7` was). Files fork patches
   (`src/cli/acp*`, `crates/jcode-tui/src/tui/app*`, `ci.yml`) are exactly the ones
   upstream churns. Backup branches are never pruned.

3. **Commit hygiene:** three commits carry real code under release/rebase titles
   (`7850606d7`, `f4459da4e`, `a9cabb825`). The ACP behavior change in
   `7850606d7` is undocumented in its message and invisible to reviewers — the
   only place it is described is fork docs `docs/acp.md:14,28` ("No fictitious
   medium default …"). Upstream still synthesizes the default
   (`upstream/master:src/cli/acp/session_configuration.rs`, `unwrap_or_else(|| …
   "medium" … efforts[0])` block, lines ~90-105).

4. **Version-suffix risk:** upstream Cargo.toml is `0.98.1` at `95bf0e557`; the
   fork occupies `0.98.2-smdex.1`. Semver orders `0.98.2-smdex.1 < 0.98.2`, so an
   upstream `v0.98.2` release will require another fork bump to stay comparable.

## Upstream-worthiness

Upstream CONTRIBUTING.md welcomes focused PRs; every PR must link an existing
issue; behavioral changes should be discussed in an issue first.

- **Propose (small, low risk):** `parse_semver` prerelease/build-metadata
  tolerance + the 4 unit tests (`35f0c6d2a`, build.rs hunks only). Upstream's
  `parse_semver` still fails on any prerelease version (returns None → base
  (0,0,0)); currently latent because upstream uses plain versions, but it is a
  correctness fix with tests. ~15 lines.
- **Propose after an issue (behavior change):** omitting the effort
  `configOption` when the daemon reports no effort (the `7850606d7` hunk).
  Changes ACP client-visible behavior; needs maintainer buy-in and should be
  extracted from the rebase commit.
- **Keep fork-local:** sync workflow + `docs/fork-maintenance.md`, version
  bumps, the `JCODE_BUILD_SEMVER` CI pin (fork CI shape differs from upstream's),
  the rustfmt/clippy hunks (they fix fork-side formatting of fork code).
- **Optional upstream PR (cosmetic):** full-semver display (`eb4b71375`) —
  behavior-neutral for upstream's plain versions; only worth bundling with #1.

## Caveats

- Run data from `gh` on 2026-10-16; scheduled runs may change the counts.
- `origin/master` (6144a6f03) is a stale legacy mirror of upstream; the live
  integration branch is `main` (4a60b9652). Local `master` tracks upstream.
