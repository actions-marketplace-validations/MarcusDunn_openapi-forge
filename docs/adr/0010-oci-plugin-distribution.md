# ADR-0010: OCI registries as the plugin distribution channel

**Status:** accepted

## Context

`forge.toml` originally only accepted filesystem paths for plugin refs:

```toml
[generator]
wasm = "../../../plugins/target/wasm32-wasip2/release/generator_typescript_fetch.wasm"
```

That's fine for in-tree development, but it forces every consumer of a
generator to vendor the `.wasm` themselves. The README and ADR-0001 both
state that "anyone can publish a generator, and anyone can run one
without trusting the author" — that promise needs a distribution
channel, not just a runtime contract.

The WASM component ecosystem has converged on OCI registries (Docker
Hub, GHCR, ECR, …) as the place to ship components. The Bytecode
Alliance defines `application/vnd.bytecodealliance.wasm.component.layer.v0+wasm`
for component-model layers; `oras push` is the de-facto publishing tool.
Adopting that convention costs us no new infrastructure and lets plugin
authors publish with tooling they already know.

## Decision

`forge.toml` accepts an `oci = "..."` field anywhere a `wasm = "..."`
field used to be valid:

```toml
[generator]
oci = "ghcr.io/marcusdunn/typescript-fetch:0.1.0"
config = { packageName = "petstore-client" }
```

The CLI pulls lazily on `forge generate`. There is no separate
`forge plugin pull` subcommand in v1; the cache makes one unnecessary.

**Library:** `oci-client` (the maintained fork of `oci-distribution`).
We use the raw OCI Distribution Spec client directly, not
`wasm-pkg-client`, because:
- The user-facing concept is "OCI ref," not "wasm-pkg name." A bare
  `ghcr.io/org/plugin:tag` is what users have in their muscle memory.
- `wasm-pkg-client` adds a layer of name resolution we'd have to
  document and configure (`wkg config` / well-known mappings).
- Smaller dependency surface — `oci-client` has roughly half the
  transitive dependency count of `wasm-pkg-client`.

**Cache:** `$XDG_CACHE_HOME/openapi-forge/plugins/`, content-addressed
by sha256 digest, with a small pointer file per tag:

```text
by-digest/sha256/<hex>.wasm        ← canonical, content-addressed
by-tag/<reg>/<repo>/<tag>.digest   ← pointer "sha256:..."
```

Refs pinned by `@sha256:...` are immutable and skip the network entirely
on cache hit. Tag-pinned refs are *mutable* (registries like GHCR do not
enforce tag immutability), so they are revalidated against the registry on
every run: forge re-resolves the tag to its wasm-layer digest with a cheap
manifest request, then serves the content-addressed blob when the digest
is unchanged or pulls the new layer when the tag has moved. This keeps
`:latest` honest without re-downloading unchanged layers. If the registry
is unreachable, forge falls back to the last cached blob for the tag
(logging a warning) so offline work isn't blocked. Pin by digest for
network-free reproducibility.

> An earlier revision of this ADR accepted serving stale tags as the cost
> of a simpler cache. That bit consumers when `:latest` was re-pushed: the
> cache kept resolving to the old build forever. Revalidation closes that
> gap while preserving the content-addressed blob store.

**Auth:** anonymous by default. For `ghcr.io` refs the CLI looks for a
GitHub token in precedence order — `GH_TOKEN`, `GITHUB_TOKEN`, then
`gh auth token` — and, if one is found, authenticates over HTTP Basic
(GHCR exchanges those credentials for a bearer token at its token
endpoint), so private GitHub packages resolve with no extra
configuration. The env vars let CI authenticate without `gh` installed
(GitHub Actions exposes `GITHUB_TOKEN`); the `gh` fallback gives local
shells the "just be logged in" experience and mirrors `gh`'s own env
precedence. If no source yields a token — env unset, `gh` missing or not
logged in — the pull degrades silently to anonymous, so public plugins
keep working without a GitHub login. A `DENIED`/
`UNAUTHORIZED` response on a `ghcr.io` ref is rewritten into an
actionable error pointing at `gh auth refresh -h github.com -s
read:packages` — the `read:packages` scope GHCR requires is not part of
the default `gh auth login` token.

For every other registry — and for `ghcr.io` when no GitHub token is
found — the CLI reads the Docker credential store: the
`$DOCKER_CONFIG/config.json` (default `~/.docker/config.json`) that
`docker login` writes, including `credsStore`/`credHelpers` credential
helpers. The contract is "if `docker pull` works, `forge` works." This
is what makes Amazon ECR — the motivating private-registry case —
resolve with zero forge-specific configuration: either
`aws ecr get-login-password | docker login` (a base64 `auths` entry) or
the `docker-credential-ecr-login` helper (a subprocess minting a fresh
token per pull), both of which users already have for Docker itself.
The credential goes over HTTP Basic, which the registry exchanges for a
bearer token per the distribution spec.

The same lookup also covers the containers-ecosystem `auth.json`
(`$REGISTRY_AUTH_FILE`, then `$XDG_RUNTIME_DIR/containers/auth.json`,
then `~/.config/containers/auth.json`), consulted after Docker's own
config. On the distros that ship `podman-docker` — NixOS, Debian,
Fedora — `docker` *is* podman, so `docker login` succeeds and writes
there; reading only `~/.docker/config.json` would break the "if
`docker pull` works, `forge` works" contract in precisely the
environment that most needs it. The schema is the same `auths` map, so
one parser serves both. Lookup walks the list and skips a file that has
no entry for the registry rather than stopping at the first file that
exists, so a leftover or logged-out `~/.docker/config.json` cannot mask a
working podman login. Docker's config stays first, so this is additive:
an existing Docker user resolves exactly what they did before.

This reader is hand-rolled (`crates/forge-cli/src/docker_auth.rs`,
~250 lines) rather than taken from the `docker_credential` crate, and
covers only the two ECR paths above. Notably the `identitytoken` field
and the `Username: "<token>"` helper convention — bearer-token
registries such as Azure ACR — are deliberately unimplemented: such a
registry degrades to an anonymous pull plus the login hint below, rather
than being silently mis-authenticated. Add them when someone needs them;
the shape of the module makes that a small change.

Access denials on non-GHCR registries get the same actionable-error
treatment as GHCR, with an ECR-aware hint
(`aws ecr get-login-password --region <region> …`, region parsed from
the registry host) and a generic `docker login <registry>` hint
elsewhere.

> An earlier revision of this ADR deferred non-GHCR auth and
> `~/.docker/config.json` reading to a follow-up; that follow-up is the
> paragraph above.

**Accepted layer media types:**
1. `application/vnd.bytecodealliance.wasm.component.layer.v0+wasm`
2. `application/wasm`
3. `application/vnd.wasm.content.layer.v1+wasm`

Single-layer artifacts whose media type is none of the above are
accepted as a pragmatic relaxation; the wasmtime instantiator will
fail loudly on non-wasm bytes anyway.

## Rationale

- **Distribution is the bottleneck on the "anyone can publish" thesis.**
  Without it, plugin authors are stuck telling users to `git clone` and
  `cargo build`. With it, `oci = "..."` is one line.
- **Determinism is preserved.** The fetch happens before the wasmtime
  engine sees a byte. The sandbox guarantees in ADR-0001 are intact —
  plugins still have no network, no clock, no env, no filesystem.
- **Caching keeps CI fast.** Content-addressed storage means a second
  `forge generate` (or a determinism-job re-run) serves the wasm layer
  from disk. Digest-pinned refs skip the network entirely; tag-pinned refs
  pay only a small manifest request to revalidate, never a layer
  re-download when the digest is unchanged.

## Consequences

- One new network surface in `forge-cli`. It is the only network surface
  in the host workspace (the parser does file I/O, plugins do nothing).
  CVE/audit scope grows by `oci-client` + `reqwest` + `rustls`.
- Reading the Docker credential store means forge may execute a
  credential-helper binary (`docker-credential-*`) found on `PATH`, but
  only one the user's own Docker config names. `base64` is the single
  new direct dependency, and it was already in the tree transitively.
- Plugin authors need a publish workflow. `oras push <ref> <plugin>.wasm:application/vnd.bytecodealliance.wasm.component.layer.v0+wasm`
  is the canonical incantation; `wkg publish` works too.
- The cache can grow without bound. We do not garbage-collect in v1;
  users can `rm -rf ~/.cache/openapi-forge` if needed. Add a
  `forge plugin gc` subcommand if this becomes a real complaint.
- `FORGE_OCI_INSECURE_HOSTS` (comma-separated host[:port] list) opts
  specific registries into plaintext HTTP. Intended for local
  registries in tests and air-gapped CI; documented in the module
  comment and the README, not advertised on the happy path.

## Alternatives considered

- **`wasm-pkg-client`.** Strong argument: it's the BA-blessed tool and
  already speaks OCI under the hood. Rejected because the abstract
  package-name layer adds UX/config surface that the v1 use case
  doesn't need. Easy to migrate later if we want broader registry
  support (warg, etc.).
- **HTTP/HTTPS direct download.** `wasm = "https://..."`. Simpler, but
  no content-addressing, no de-facto signature/digest pinning, and
  ignores the ecosystem's convergence on OCI.
- **`forge plugin install` ahead-of-time.** A separate subcommand that
  manages a lockfile. Worth doing later if reproducibility on tag
  drift becomes a pain point. Pinning by digest covers it for now.
- **Bundling generators in the host.** The thing ADR-0001 explicitly
  rejected. Repeating the rejection for completeness.
