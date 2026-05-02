# Release pipeline

End-to-end flow when a release-triggering commit lands on `main`:

1. `release-please.yml` opens or updates a "Release PR" with the next
   version bump and CHANGELOG entry.
2. You merge the Release PR.
3. release-please pushes the `vX.Y.Z` tag.
4. `release.yml` fires on the tag push:
   - Builds binaries for linux, macOS (aarch64), and windows.
   - Attaches the tarballs / zip to the GitHub Release.
   - Publishes every workspace crate to crates.io (idempotent — already-
     published versions are skipped, so a partial release-please bump
     doesn't nuke the run).

For step 4 to fire automatically on step 3, release-please must push
the tag using a **personal access token (PAT)**, not the default
`GITHUB_TOKEN`. GitHub policy: tags pushed by `GITHUB_TOKEN` do NOT
trigger downstream workflows. Without the PAT, the flow stalls between
step 3 and 4 and you have to dispatch `release.yml` manually.

## One-time PAT setup

Mint a fine-grained PAT once and store it as a repo secret. The
release-please workflow falls back to `GITHUB_TOKEN` if the secret is
missing, so the repo still works without the PAT — it just won't
auto-trigger the release pipeline.

1. **Create the PAT.** Go to
   <https://github.com/settings/personal-access-tokens/new>.
   - **Resource owner:** your account.
   - **Repository access:** `Only select repositories` → `bouncy`.
   - **Repository permissions:**
     - `Contents: Read and write` (push tags + commits)
     - `Pull requests: Read and write` (open / update the Release PR)
     - `Workflows: Read and write` (touch `.github/workflows/*` if a
       Release PR ever needs to)
   - **Expiration:** 1 year is reasonable; calendar a renewal.

2. **Store it as a repo secret.**

   ```
   gh secret set RELEASE_PLEASE_TOKEN --repo maziarzamani/bouncy
   # paste the PAT when prompted
   ```

   Or via the web UI: <https://github.com/maziarzamani/bouncy/settings/secrets/actions/new>.

3. **Verify.** Land any `feat:` / `fix:` commit on main and watch the
   Actions tab — the release-please run should open a Release PR. When
   you merge it, the resulting tag push should fire `release.yml`
   automatically (you'll see two workflow runs: release-please's commit
   onto main, then release.yml on the tag).

## Re-dispatching a stale release

`workflow_dispatch --ref vX.Y.Z` reads the `release.yml` file from the
**tagged commit**. If a workflow fix landed on main *after* the tag was
created, that fix is not present on the tag, so the dispatch runs the
old workflow.

To re-run a release with a workflow fix that landed after the tag:

```
git tag -f vX.Y.Z origin/main          # move tag to a commit with the fix
git push -f origin vX.Y.Z              # admin bypass on v* protection
                                       # (push event auto-fires release.yml)
```

The push event reads `release.yml` from the new tagged commit, so the
fix applies. This is the only path that doesn't require cutting a new
version number to ship a workflow fix against an old release.

## Crates.io token

`CRATES_IO_TOKEN` repo secret is used by `release.yml`'s publish step.
Mint at <https://crates.io/me> → "API Tokens". Scope `publish-update`
restricted to the seven `bouncy-*` crates is sufficient. Same renewal
discipline as the PAT.

## Version bumps

In pre-1.0 with the current `release-please-config.json`
(`bump-minor-pre-major: true`, `bump-patch-for-minor-pre-major: true`),
`feat:` commits bump PATCH (0.1.3 → 0.1.4), not MINOR. Once the project
hits 1.0, `feat:` will bump MINOR and `fix:` will bump PATCH per
standard SemVer.

The `linked-versions` plugin only aligns crates that already received
a bump-triggering commit in their own path. A `feat:` only under
`crates/bouncy-cli/` will bump cli alone; the lib crates stay at their
previous version. The publish job's idempotent guard handles this
cleanly — unchanged crates are skipped.
