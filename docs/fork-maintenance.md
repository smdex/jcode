# Fork maintenance

The `smdex/jcode` fork keeps customizations on `main`. Its upstream is
`1jehuang/jcode`, branch `master`.

`.github/workflows/sync-upstream.yml` runs daily at 04:23 UTC and can be run
manually. It creates a `sync-backup/<run-id>-<attempt>` recovery branch before
rebasing, preserves merges, aborts on conflicts, and updates `main` only with
an exact force-with-lease. A concurrent change to `main` therefore rejects the
push rather than losing that change. Conflicts require manual resolution.

Publishing a workflow file requires GitHub credentials with workflow access.
For automatic syncs that include upstream workflow changes, configure an
`UPSTREAM_SYNC_TOKEN` Actions secret with repository contents and workflow
write access. Otherwise the workflow falls back to `GITHUB_TOKEN`, which may
reject those updates. Branch protection must permit the intended rebase push.
The workflow does not run the full build suite or deploy binaries. Review the
existing CI results before adopting an updated revision in Nix.

After an automatic rebase, update a local checkout without replaying the old
customization commits again:

```sh
git fetch origin
# First commit or stash local changes, then retain the old history:
git branch backup/local-before-sync
git switch main
git reset --hard origin/main
```

Only run the reset after preserving all local work. A previous remote state is
also available in the workflow's recovery branch, reported in its run summary.
