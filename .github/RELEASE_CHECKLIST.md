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
- `openapi-forge-cli`
- `forge-plugin-sdk`

Trusted Publishing only authorizes crates that already exist on crates.io. If
any of the above are not yet published, do a one-time manual `cargo publish`
of `0.1.0` first to seed them.

### GitHub repo settings

- **Environment**: create `release` (Settings → Environments → New environment).
  - Deployment branch rule: `main` only.
  - (Optional but recommended) required reviewer = a maintainer.

The prep-next-release PR is opened with the default `GITHUB_TOKEN`, which
means GitHub will not automatically trigger CI on it. The bump is mechanical
(version-only) so this is acceptable; if you want CI to run before merging,
push an empty commit to the branch, or close-and-reopen the PR.

### GHCR plugin packages

The `publish-plugins-oci` job pushes two minimal test-fixture plugins to
GHCR on every release so the CLI's OCI pull path always has a stable
public artifact to exercise:

- `ghcr.io/marcusdunn/transformer-noop`
- `ghcr.io/marcusdunn/generator-debug-dump`

The first run of the workflow creates each package as **private**. The CLI
uses `RegistryAuth::Anonymous` (`crates/forge-cli/src/oci.rs:169`), so an
anonymous pull will 401 until the package is public. After the first
release that publishes a given plugin, do this once per plugin:

1. GitHub → your profile → **Packages** → `<plugin>` → **Package settings**.
2. **Change visibility** → **Public**.
3. **Manage Actions access** → ensure `openapi-forge` has `Write`
   (the default usually works; set this explicitly if package ownership
   ever moves to an org).

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
   - `publish-plugins-oci` runs in parallel with `prep-next` after
     `publish` completes; pushes the two test-fixture plugins to GHCR
     (~30s each, best-effort — failures here do not block the release).
   - `prep-next` opens a PR titled `chore: prep next release v<next>` once
     publish completes.
7. Verify on crates.io that all 8 versions landed.
8. Verify provenance locally on at least one crate:
   ```
   gh release download "vX.Y.Z" --pattern 'forge-ir-X.Y.Z.crate'
   gh attestation verify forge-ir-X.Y.Z.crate --owner marcusdunn
   ```
9. Verify the GHCR plugin artifacts. Create a throwaway project dir with
   a `forge.toml` that references both via `oci = "..."` and run
   `forge generate`:
   ```
   mkdir /tmp/forge-oci-smoke && cd /tmp/forge-oci-smoke
   cat >forge.toml <<EOF
   [input]
   spec = "openapi.json"

   [[transformers]]
   oci = "ghcr.io/marcusdunn/transformer-noop:X.Y.Z"

   [generator]
   oci = "ghcr.io/marcusdunn/generator-debug-dump:X.Y.Z"

   [output]
   dir = "out"
   EOF
   cp /path/to/some/openapi.json .
   rm -rf ~/.cache/openapi-forge/plugins   # force a real network pull
   forge generate
   ls out/ir.txt                            # generator-debug-dump output
   gh attestation verify oci://ghcr.io/marcusdunn/transformer-noop:X.Y.Z   --owner marcusdunn
   gh attestation verify oci://ghcr.io/marcusdunn/generator-debug-dump:X.Y.Z --owner marcusdunn
   ```
   Pin by digest (`oci = "...@sha256:..."`) for repeated runs — the digests
   are surfaced in the `publish-plugins-oci` run summary.
10. Review and merge the prep-next PR. CI does not run on it automatically
    (it's opened by `GITHUB_TOKEN`); if you want CI before merging, push an
    empty commit or close-and-reopen.

## Recovering from a failed release

### Plugin OCI push failed

`publish-plugins-oci` is `continue-on-error: true`, so a failure here
does not block the release or the prep-next PR. Re-run the failed leg
of the matrix (Actions → Release → Re-run failed jobs) after fixing the
underlying issue. `oras push` is idempotent on identical bytes; if a
partial referrer (provenance) manifest was written, you may need a PAT
with `delete:packages` to clean it up before re-push. Worth automating
only if this becomes recurrent.

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

- Plugins under `plugins/` are not published to crates.io. Two of them —
  `transformer-noop` and `generator-debug-dump` — are published to GHCR
  by `publish-plugins-oci` as test fixtures for the CLI's OCI pull path.
  The remaining plugins are unpublished.
- The fuzz workspace (`fuzz/`, nightly-only).
- `forge-plugin-itests`, `xtask`.
