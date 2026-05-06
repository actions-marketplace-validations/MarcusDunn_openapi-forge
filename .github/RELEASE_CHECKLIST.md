# Release Checklist

Operator runbook for cutting a new release of openapi-forge. The publish
mechanics are automated by `.github/workflows/release.yml`; this doc covers
the human steps.

## One-time setup (do once, then never again)

### crates.io Trusted Publishing

For each crate below, log in to crates.io and configure a Trusted Publisher
(Settings → Trusted Publishers → Add GitHub Actions):

- Owner: `marcusdunn`
- Repository: `openapi-forge`
- Workflow filename: `release.yml`
- Environment: `release`

Crates:

- `forge-ir`
- `forge-parser`
- `forge-ir-bindgen`
- `forge-host`
- `forge-pipeline`
- `forge-test-harness`
- `forge-cli`
- `forge-plugin-sdk`

Trusted Publishing only authorizes crates that already exist on crates.io. If
any of the above are not yet published, do a one-time manual `cargo publish`
of `0.1.0` first to seed them.

### GitHub repo settings

- **Environment**: create `release` (Settings → Environments → New environment).
  - Deployment branch rule: `main` only.
  - (Optional but recommended) required reviewer = a maintainer.
- **Secret**: `RELEASE_BOT_TOKEN`. Either a PAT with `repo` + `workflow` scopes
  or a GitHub App installation token. Required so the prep-next-release PR
  triggers CI — the default `GITHUB_TOKEN` does not.
- **Branch protection** on `main`: require CI to pass before merging
  `release-prep` PRs.

## Cutting a release

1. Confirm `main` is green.
2. Confirm the workspace version in `Cargo.toml` (and the matching version in
   `crates/forge-plugin-sdk/Cargo.toml`) is the version you intend to release.
   This is the version landed by the prep-next PR after the previous release;
   it should already be correct.
3. Update the `## [Unreleased]` section in `CHANGELOG.md` with notes for this
   release. Don't rename the heading — the workflow does that automatically.
4. Tag `vX.Y.Z` matching the workspace version, push the tag, and draft a
   GitHub release pointing at it.
5. Click **Publish release**.
6. Watch the `Release` workflow:
   - `verify` should pass within a minute.
   - `publish` runs sequentially through the 8 crates; total ~5–10 minutes.
   - `prep-next` opens a PR titled `chore: prep next release v<next>` once
     publish completes.
7. Verify on crates.io that all 8 versions landed.
8. Verify provenance locally on at least one crate:
   ```
   gh release download "vX.Y.Z" --pattern 'forge-ir-X.Y.Z.crate'
   gh attestation verify forge-ir-X.Y.Z.crate --owner marcusdunn
   ```
9. Review and merge the prep-next PR. CI should run on it (because of
   `RELEASE_BOT_TOKEN`); if it doesn't, the token is misconfigured.

## Recovering from a failed release

### Partial publish (e.g. crate 5/8 failed)

The publish action's idempotency probe checks crates.io for each `name@version`
before publishing. Re-running the workflow (Actions → Release → Re-run failed
jobs) skips already-published crates and resumes at the failed one. No version
bump needed.

### Hard rejection (crates.io refused the upload)

Do not try to "fix" the existing tag — crates.io versions are immutable. Bump
the version (likely a patch), tag a new release, and start over. The
prep-next PR from the most recent successful release should already be on
`main`; pick the next patch from there.

### Trusted Publisher misconfigured

If a crate's TP entry is missing or wrong, the workflow fails with a 401/403
from crates.io. Fix the TP config on crates.io and re-run the failed job.

## Exercising the pipeline without publishing

Use **Run workflow** (Actions → Release → Run workflow) with `dry_run: true`.
This runs `cargo package` + `cargo publish --dry-run` for every crate and
emits attestations (which are free), but does not publish and does not open a
prep-next PR. Useful for validating workflow changes before a real release.

## Out of scope

- Plugins under `plugins/` (`publish = false`, target `wasm32-wasip2` only).
- The fuzz workspace (`fuzz/`, nightly-only).
- `forge-plugin-itests`, `xtask`.
