# OpenAPI Forge

A WASM-plugin-based replacement for `openapi-generator`.

The host parses OpenAPI specs into a normalized intermediate representation,
then runs sandboxed WebAssembly plugins that either transform the IR or emit
code from it. Plugins are untrusted by default; the host enforces capability
limits — no filesystem, no network, no clock, no randomness, bounded fuel and
memory.

> **Status: Stage 1 / pre-alpha.** The WIT contract, IR types, and workspace
> are in place. The parser and runtime land in subsequent phases. There is
> no working CLI yet.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/MarcusDunn/openapi-forge/main/install.sh | sh
```

Downloads the right binary for your platform, verifies SHA-256 checksums,
and checks [SLSA build provenance](https://slsa.dev) if `gh` is installed.
Installs to `~/.local/bin` by default.

Pin a version or change the install directory:

```sh
FORGE_VERSION=0.1.11 FORGE_INSTALL_DIR=/usr/local/bin \
  curl -fsSL https://raw.githubusercontent.com/MarcusDunn/openapi-forge/main/install.sh | sh
```

Or install from source via crates.io:

```sh
cargo install openapi-forge-cli
```

### Nix

The flake exposes the `forge` CLI as its default package, so you can run it
ad-hoc or pin it into a system/home-manager flake and bump it with
`nix flake update`:

```sh
# Run without installing
nix run github:MarcusDunn/openapi-forge -- --help

# Install into a profile
nix profile install github:MarcusDunn/openapi-forge
```

To consume it from another flake, add it as an input and reference the
package. The input tracks `main` (which carries the version bump for each
release), so `nix flake update` advances it to the latest commit:

```nix
{
  inputs.openapi-forge.url = "github:MarcusDunn/openapi-forge";

  # then, e.g. in environment.systemPackages / home.packages:
  #   inputs.openapi-forge.packages.${pkgs.system}.default
}
```

Pin a specific release instead by pointing the input at a tag
(`github:MarcusDunn/openapi-forge/v0.1.21`); a versioned tag never moves, so
you bump it by hand.

## Why

`openapi-generator` (the Swagger project) requires generators to ship in-tree.
Adding or modifying one means a PR upstream, navigating their backwards-
compatibility constraints, and shipping on their release cadence. The result
is a long tail of unmaintained or near-unusable generators with no escape
hatch for users.

OpenAPI Forge inverts that: generators are plugins, parsing is in-tree.
Anyone can publish a generator, and anyone can run one without trusting the
author — the WASM sandbox enforces what the plugin can and can't do. The
host owns parsing, normalization, output validation, and determinism.

## Reading order

- `docs/architecture.md` — for contributors
- `docs/ir-spec.md` — the canonical IR contract
- `docs/plugin-authoring.md` — for plugin authors
- `docs/adr/` — architecture decision records

## Plugin sources

`forge.toml` accepts plugins from the local filesystem or any OCI
registry (Docker Hub, GHCR, ECR, …). Both forms can be mixed freely;
the `config = { ... }` block applies to either.

```toml
[input]
spec = "openapi.json"

# Filesystem ref — useful for in-tree plugin development.
[[transformers]]
wasm = "./plugins/my-transformer.wasm"

# OCI ref — pulled lazily on `forge generate`, cached under the user's
# XDG cache dir. Mutable tags like `:latest` are revalidated against the
# registry each run; pin by digest (`@sha256:…`) for airtight,
# network-free reproducibility.
[generator]
oci = "ghcr.io/marcusdunn/typescript-fetch:0.1.0"
config = { packageName = "petstore-client" }

[output]
dir = "out"
```

Pulls are anonymous by default. For private packages, `forge` reads
credentials from two sources:

**Any registry** (Amazon ECR, Docker Hub, self-hosted, …): the Docker
credential store — the same `~/.docker/config.json` that `docker login`
writes, including `credsStore`/`credHelpers` credential helpers. If
`docker pull` works, `forge` works. For ECR either log in with the AWS
CLI:

```sh
aws ecr get-login-password --region us-east-1 | \
  docker login --username AWS --password-stdin 123456789012.dkr.ecr.us-east-1.amazonaws.com
```

or configure [`docker-credential-ecr-login`](https://github.com/awslabs/amazon-ecr-credential-helper)
once and forget about token expiry — `forge` invokes the helper on each
pull, just like Docker does.

**Using podman?** If `docker` on your machine is really podman (the
`podman-docker` package on NixOS, Debian and Fedora), `docker login`
writes `containers/auth.json` rather than `~/.docker/config.json`.
`forge` reads that too, so the plain login above just works. Files are
consulted in this order, first match winning:

| # | Path |
|---|------|
| 1 | `$DOCKER_CONFIG/config.json` (default `~/.docker/config.json`) |
| 2 | `$REGISTRY_AUTH_FILE` |
| 3 | `$XDG_RUNTIME_DIR/containers/auth.json` |
| 4 | `~/.config/containers/auth.json` |

A file with no entry for the registry is skipped rather than ending the
search, so a leftover empty `~/.docker/config.json` will not mask a
working podman login.

**`ghcr.io`**: additionally, log in with the
[GitHub CLI](https://cli.github.com/) and `forge` reuses that token
automatically — no separate configuration. GHCR package reads need the
`read:packages` scope, which the default login does not grant:

```sh
gh auth refresh -h github.com -s read:packages
```

If a private pull is denied, `forge` prints the exact login command for
the registry involved.

**In CI**, where `gh` may not be installed, set `GH_TOKEN` (or
`GITHUB_TOKEN`) instead — `forge` reads it directly, no `gh` required.
Tokens are checked in the order `GH_TOKEN`, `GITHUB_TOKEN`, then
`gh auth token`, then the Docker credential store.

```yaml
# GitHub Actions
permissions:
  packages: read            # required for the built-in GITHUB_TOKEN
steps:
  - run: forge generate ...
    env:
      # built-in token works for packages the running repo can access;
      # use a PAT with read:packages for other repos/orgs
      GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

See `docs/adr/0010-oci-plugin-distribution.md` for the cache layout,
accepted layer media types, and roadmap.

### Config-less invocation

For one-off runs (CI scripts, ad-hoc generation), pass the pipeline shape
directly on the command line and skip `forge.toml` entirely:

```sh
forge generate \
  -i openapi.json \
  --transformer ./plugins/my-transformer.wasm \
  --transformer ghcr.io/example/redact:1.2.0 \
  --generator  ghcr.io/marcusdunn/typescript-fetch:0.1.0 \
  -o out
```

`--transformer` is repeatable and runs in order. Each `--transformer` /
`--generator` value is auto-detected as a filesystem path or an OCI ref
(if it ends in `.wasm` or names an existing file, it's a path).
Per-plugin config defaults to `{}`; reach for `forge.toml` when you need
to pass `config = { ... }` blocks.

## Multiple pipelines

One `forge.toml` can drive several independent transforms → generator
stacks. Declare each as a `[[pipelines]]` table with its own `[output]`;
the top-level `[input]` and `[limits]` act as shared defaults that any
pipeline may override. A single `forge generate` runs them all in order.

```toml
# Shared by every pipeline unless a pipeline overrides it.
[input]
spec = "openapi.json"

[[pipelines]]
name = "typescript"            # optional label, shown in the run summary

[[pipelines.transformers]]
wasm = "./plugins/redact.wasm"

[pipelines.generator]
oci = "ghcr.io/marcusdunn/typescript-fetch:0.1.0"
config = { packageName = "petstore-client" }

[pipelines.output]
dir = "out/ts"

[[pipelines]]
name = "rust"

[pipelines.generator]
wasm = "./plugins/rust-reqwest.wasm"

[pipelines.output]
dir = "out/rust"

[pipelines.hooks]
post_generate = ["cargo fmt"]

# Optional: layer per-pipeline limit overrides on top of the top-level
# [limits]. Anything unset falls back to the shared value, then the
# built-in default.
[pipelines.limits.generator]
wall_clock_ms = 60000
```

A pipeline may set its own `[pipelines.input]` to read a different spec or
pre-parsed IR; otherwise it inherits the top-level `[input]`. Hooks are
per-pipeline: each `[[pipelines]]` declares its own `[pipelines.hooks]`,
run against that pipeline's output dir.

This is mutually exclusive with the single-pipeline layout above — a
manifest that defines both top-level `[generator]`/`[[transformers]]`/
`[output]`/`[hooks]` *and* `[[pipelines]]` is rejected as ambiguous. `--out` is
likewise rejected with multiple pipelines, since each one writes to its
own `[output] dir`.

## Sandbox limits

Plugins run under per-stage resource limits (fuel, memory, wall-clock,
output size). The defaults suit typical specs; very large specs or
prolific generators can hit them. Raise (or tighten) any limit with an
optional `[limits]` section in `forge.toml` — unset keys keep their
defaults:

```toml
[limits.transformer]
fuel = 10_000_000_000        # default 5_000_000_000
memory_bytes = 268_435_456   # default 128 MiB
wall_clock_ms = 10_000       # default 5_000

[limits.generator]
fuel = 100_000_000_000             # default 50_000_000_000
memory_bytes = 1_073_741_824       # default 512 MiB
wall_clock_ms = 60_000             # default 30_000
output_files_max = 50_000          # default 10_000
output_total_bytes_max = 1_073_741_824    # default 256 MiB
output_per_file_bytes_max = 134_217_728   # default 16 MiB
```

The `output_*` caps apply only to generators — transformers return IR,
not files. Misspelled keys are rejected rather than silently ignored.

## Post-generation hooks

Run commands after generation finishes — typically formatters over the
generated code — with an optional `[hooks]` section in `forge.toml`:

```toml
[hooks]
post_generate = [
  "eslint --fix && prettier --write .",                   # shell form
  ["cargo", "fmt"],                                        # exec form
  { cmd = "optional-linter", continue_on_error = true },  # table form
]
```

A hook's command is given in one of two forms (cf. Docker's shell vs exec
form):

- **shell form** — a string run through the platform shell. Globs
  (`*.ts`), pipes, `&&`, redirection and `$VAR` expansion all work.
- **exec form** — an argv array run directly with **no shell**. Arguments
  pass through literally (no word-splitting or glob/var expansion) and no
  shell needs to be present. Prefer this for paths with spaces or fully
  deterministic invocations.

An entry is either a bare command (string or array) or a **table** that
wraps a command (`cmd = <string|array>`) with per-hook options:

- `continue_on_error` (default `false`) — when `true`, a non-zero exit or
  a failure to start logs a warning and continues to the next hook
  instead of aborting. Use it for optional or best-effort hooks.

Commands run in order, only after every generated file is written, with
the **output directory as their working directory**. Two **absolute**
paths are exported so a hook can anchor arguments regardless of its cwd:

- `FORGE_OUT_DIR` — the directory the generated files were written to.
- `FORGE_MANIFEST_DIR` — the directory containing `forge.toml`.

stdout/stderr are inherited so formatter output is visible. For example, a
hook can point a formatter at a config kept next to the manifest:

```toml
post_generate = ['oxfmt -c "$FORGE_MANIFEST_DIR/.oxfmtrc.json" "$FORGE_OUT_DIR/client.ts"']
```

A hook that fails (and isn't marked `continue_on_error`) aborts the run
and `forge` exits **3** — distinct from the exit **2** used for forge's
own errors, so callers can tell "a hook failed" (generation succeeded;
files are on disk) apart from "generation failed".

Hooks run in project mode only; [config-less invocation](#config-less-invocation)
has no `[hooks]` section. Misspelled keys are rejected.

The full project plan lives in the issue tracker / project documentation;
this README intentionally summarises rather than duplicates.

## Platform support

macOS and Linux. **No Windows.** See plan §2.

## License

Apache-2.0 OR MIT.
