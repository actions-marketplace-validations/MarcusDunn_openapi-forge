//! Pull WASM-component plugins from OCI registries.
//!
//! This module is the only network surface in `forge-cli`. The host
//! sandbox forbids plugins from doing any I/O, but the *act of fetching*
//! the plugin happens here, before the wasmtime engine ever sees the
//! bytes. After fetch, the bytes flow into `Plugin::load_*` exactly as
//! they would for a filesystem ref.
//!
//! The user-facing surface is one function: [`fetch_to_bytes`]. It
//! parses an OCI reference, consults a content-addressed cache under
//! the user's XDG cache dir, and pulls from the registry on miss. Pulls
//! are anonymous in v1 — see ADR-0010.
//!
//! Cache layout (`$XDG_CACHE_HOME/openapi-forge/plugins/`):
//!
//! ```text
//! by-digest/
//!   sha256/
//!     <hex>.wasm                    ← canonical, content-addressed
//! by-tag/
//!   <registry>/<repo>/<tag>.digest  ← tiny pointer file with "sha256:..."
//! ```
//!
//! Refs pinned by `@sha256:...` are immutable and skip the network
//! entirely on cache hit. Tag-pinned refs are *mutable* — `:latest` (and
//! any other tag, since registries like GHCR do not enforce tag
//! immutability) can be re-pushed to a new digest at any time — so on
//! every run we re-resolve the tag against the registry with a cheap
//! manifest request and compare digests. The expensive wasm layer is
//! still served from the content-addressed blob store whenever the digest
//! is unchanged, so revalidation only pays for a manifest round-trip, not
//! a layer download. If the registry can't be reached we fall back to the
//! last cached blob for the tag (with a warning) so offline and transient
//! failures don't block generation. Pin by digest for airtight,
//! network-free reproducibility.
//!
//! ## Auth
//!
//! Pulls are anonymous by default. Credentials, when needed, come from
//! two sources:
//!
//! - **`ghcr.io`**: a GitHub token in precedence order — `GH_TOKEN`,
//!   `GITHUB_TOKEN`, then `gh auth token` — authenticated over HTTP
//!   Basic so private GitHub packages resolve. The env vars let CI
//!   authenticate without `gh` installed; the `gh` fallback gives local
//!   shells the "just be logged in" experience.
//! - **Any registry**: the Docker credential store — the same
//!   `$DOCKER_CONFIG/config.json` (default `~/.docker/config.json`)
//!   that `docker login` writes, including `credsStore`/`credHelpers`
//!   credential helpers such as `docker-credential-ecr-login` for
//!   Amazon ECR, plus podman's `containers/auth.json`. See
//!   [`crate::docker_auth`]. For `ghcr.io` this is a fallback behind the
//!   GitHub token sources above; everywhere else it is the only source.
//!
//! If no source yields a credential, the pull degrades silently to
//! anonymous — public plugins keep working with no login. See ADR-0010.

use std::path::{Path, PathBuf};

use oci_client::{
    client::{ClientConfig, ClientProtocol},
    manifest::OciImageManifest,
    secrets::RegistryAuth,
    Client, Reference,
};
use sha2::{Digest, Sha256};

/// Registry whose private packages we can unlock with a GitHub token
/// (from the environment or the `gh` CLI).
const GHCR_REGISTRY: &str = "ghcr.io";

/// Username sent alongside the `gh` token over HTTP Basic. GHCR's token
/// endpoint validates only the password (the token) and ignores the
/// username, so this is a descriptive placeholder mirroring git's
/// token-as-password convention.
const GHCR_TOKEN_USERNAME: &str = "x-access-token";

/// Layer media types we accept as carrying a single WASM component.
/// First match wins; order is informational only.
const ACCEPTED_MEDIA_TYPES: &[&str] = &[
    // Bytecode Alliance convention for component-model wasm layers.
    "application/vnd.bytecodealliance.wasm.component.layer.v0+wasm",
    // Generic wasm. Used by `oras push` defaults and several existing
    // registries.
    "application/wasm",
    // Wider OCI-Wasm proposal media type some pushers use.
    "application/vnd.wasm.content.layer.v1+wasm",
];

#[derive(Debug, thiserror::Error)]
pub enum OciError {
    #[error("invalid OCI reference {reference:?}: {source}")]
    BadRef {
        reference: String,
        #[source]
        source: oci_client::ParseError,
    },
    #[error("could not determine cache directory")]
    NoCacheDir,
    #[error("cache I/O at {path}: {source}")]
    CacheIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("registry pull: {0}")]
    Registry(#[from] oci_client::errors::OciDistributionError),
    #[error("access denied pulling {reference}. {hint}")]
    AccessDenied {
        reference: String,
        /// Registry-specific login hint, see [`access_denied_hint`].
        hint: String,
        // Boxed to keep the enum (and every Result that carries it)
        // under clippy's result_large_err threshold.
        #[source]
        source: Box<oci_client::errors::OciDistributionError>,
    },
    #[error(
        "no acceptable wasm layer in {reference}: \
         expected one of [{}], got [{got}]",
        ACCEPTED_MEDIA_TYPES.join(", ")
    )]
    NoWasmLayer { reference: String, got: String },
    #[error("digest mismatch for {reference}: expected {expected}, got {actual}")]
    DigestMismatch {
        reference: String,
        expected: String,
        actual: String,
    },
    #[error("unsupported digest algorithm in {0}: only sha256 is supported")]
    UnsupportedDigestAlgo(String),
    #[error("async runtime: {0}")]
    Runtime(std::io::Error),
}

/// Fetch the wasm component bytes for an OCI reference, populating the
/// on-disk cache. Returns the bytes ready to hand to `Plugin::load_*`.
pub fn fetch_to_bytes(reference: &str) -> Result<Vec<u8>, OciError> {
    let parsed: Reference = reference.parse().map_err(|e| OciError::BadRef {
        reference: reference.to_owned(),
        source: e,
    })?;

    let cache_root = cache_root()?;

    // Fast path: digest-pinned refs are immutable, so a cached blob is
    // authoritative — skip the network entirely on a hit.
    if let Some(digest) = parsed.digest() {
        if let Some(bytes) = read_blob_by_digest(&cache_root, digest)? {
            tracing::debug!(target: "forge::oci", %reference, "cache hit (digest-pinned)");
            return Ok(bytes);
        }
    }

    // Resolve auth synchronously (it may shell out to `gh` or a Docker
    // credential helper) before entering the async runtime, keeping the
    // subprocess off the executor.
    let auth = resolve_auth(parsed.registry());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(OciError::Runtime)?;

    // Tag-pinned refs are mutable: re-resolve the tag against the registry
    // every run so a re-pushed `:latest` is never served stale. The wasm
    // layer is still served from the content-addressed blob cache when the
    // digest hasn't moved, so this costs only a cheap manifest request.
    if parsed.digest().is_none() {
        if let Some(tag) = parsed.tag() {
            match runtime.block_on(resolve_layer_digest(&parsed, &auth)) {
                Ok(layer_digest) => {
                    if let Some(bytes) = read_blob_by_digest(&cache_root, &layer_digest)? {
                        // Tag still resolves to a build we already have on
                        // disk: refresh the pointer and serve from cache
                        // without re-downloading the layer.
                        write_tag_pointer(
                            &cache_root,
                            parsed.registry(),
                            parsed.repository(),
                            tag,
                            &layer_digest,
                        )?;
                        tracing::debug!(
                            target: "forge::oci",
                            %reference, %layer_digest,
                            "revalidated tag against registry; cache hit",
                        );
                        return Ok(bytes);
                    }
                    // Tag moved to a build we don't have cached — fall
                    // through to a full pull below.
                    tracing::debug!(
                        target: "forge::oci",
                        %reference, %layer_digest,
                        "tag resolved to a new digest; pulling layer",
                    );
                }
                Err(e) => {
                    // The registry could not be reached (offline, a
                    // transient failure, or the package was removed). Fall
                    // back to the last known-good blob for this tag so work
                    // isn't blocked — but make the staleness visible.
                    if let Some(digest) =
                        read_tag_pointer(&cache_root, parsed.registry(), parsed.repository(), tag)?
                    {
                        if let Some(bytes) = read_blob_by_digest(&cache_root, &digest)? {
                            tracing::warn!(
                                target: "forge::oci",
                                %reference, error = %e,
                                "could not revalidate tag against registry; \
                                 serving cached (possibly stale) plugin — pin by \
                                 digest or clear the cache if this is unexpected",
                            );
                            return Ok(bytes);
                        }
                    }
                    return Err(e);
                }
            }
        }
    }

    // Cache miss (tag moved, never cached, or a bare ref with no tag):
    // pull the layer bytes from the registry.
    let pulled = runtime.block_on(pull(&parsed, &auth))?;

    // If the caller pinned by digest, validate it before trusting the
    // bytes. The OCI client validates layer digests against the manifest,
    // but we want to validate against the *user-supplied* digest too.
    if let Some(expected) = parsed.digest() {
        let actual = layer_digest(&pulled.bytes);
        if !digests_equal(expected, &actual) {
            return Err(OciError::DigestMismatch {
                reference: reference.to_owned(),
                expected: expected.to_owned(),
                actual,
            });
        }
    }

    write_blob(&cache_root, &pulled.layer_digest, &pulled.bytes)?;
    if let Some(tag) = parsed.tag() {
        write_tag_pointer(
            &cache_root,
            parsed.registry(),
            parsed.repository(),
            tag,
            &pulled.layer_digest,
        )?;
    }

    tracing::info!(
        target: "forge::oci",
        %reference,
        digest = %pulled.layer_digest,
        bytes = pulled.bytes.len(),
        "pulled plugin from registry",
    );
    Ok(pulled.bytes)
}

struct Pulled {
    bytes: Vec<u8>,
    /// Digest of the wasm layer, e.g. `sha256:...`. Used as the cache key.
    layer_digest: String,
}

async fn pull(reference: &Reference, auth: &RegistryAuth) -> Result<Pulled, OciError> {
    let client = new_client();
    let image = client
        .pull(reference, auth, ACCEPTED_MEDIA_TYPES.to_vec())
        .await
        .map_err(|e| map_registry_error(reference, e))?;

    let layer = pick_wasm_layer(&image.layers, &image.manifest, &reference.to_string())?;
    let bytes = layer.data.to_vec();
    let layer_digest = layer_digest(&bytes);
    Ok(Pulled {
        bytes,
        layer_digest,
    })
}

/// Re-resolve a (mutable) tag against the registry to the digest of its
/// wasm layer, *without* downloading the layer itself. This is the cheap
/// manifest request that lets [`fetch_to_bytes`] notice when a tag like
/// `:latest` has been re-pushed since it was last cached.
///
/// The returned value is the layer descriptor's digest. For the
/// uncompressed `application/wasm` layers forge publishes, that equals the
/// sha256 of the layer bytes — i.e. the key the blob cache is stored under
/// (see [`layer_digest`] / [`write_blob`]). When a tag is re-pushed the
/// digest changes, so the subsequent blob lookup misses and the caller
/// pulls the new build. (If a registry ever served a *compressed* layer
/// whose descriptor digest differs from the byte sha256, the lookup simply
/// misses and we fall through to a correct full pull — never a wrong hit.)
async fn resolve_layer_digest(
    reference: &Reference,
    auth: &RegistryAuth,
) -> Result<String, OciError> {
    let client = new_client();
    let (manifest, _manifest_digest) = client
        .pull_image_manifest(reference, auth)
        .await
        .map_err(|e| map_registry_error(reference, e))?;

    let layer = pick_wasm_descriptor(&manifest.layers, &reference.to_string())?;
    Ok(layer.digest.clone())
}

/// Build an OCI client honouring [`configured_protocol`]. Shared by the
/// full-pull and manifest-only revalidation paths so they speak to the
/// registry the same way.
fn new_client() -> Client {
    Client::new(ClientConfig {
        protocol: configured_protocol(),
        ..ClientConfig::default()
    })
}

/// Map a raw registry error into [`OciError`], upgrading an
/// access-denial into an actionable, registry-specific login hint. An
/// opaque 401/403 otherwise leaves the user chasing the wrong thing.
fn map_registry_error(
    reference: &Reference,
    e: oci_client::errors::OciDistributionError,
) -> OciError {
    if is_access_denied(&e) {
        OciError::AccessDenied {
            reference: reference.to_string(),
            hint: access_denied_hint(reference.registry()),
            source: Box::new(e),
        }
    } else {
        e.into()
    }
}

/// The login instruction appended to an access-denied error, specialised
/// per registry so the user is pointed at the credential source forge
/// actually reads:
/// - `ghcr.io`: the `gh` CLI needs the `read:packages` scope, which the
///   default `gh auth login` token lacks.
/// - Amazon ECR (private or public): a `docker login` fed by the AWS CLI
///   (or the `ecr-login` credential helper).
/// - anything else: plain `docker login`, since forge reads the Docker
///   credential store.
fn access_denied_hint(registry: &str) -> String {
    if registry == GHCR_REGISTRY {
        return "If this is a private package, log in with the GitHub CLI and \
                ensure your token carries the `read:packages` scope:\n    \
                gh auth refresh -h github.com -s read:packages\n  \
                (or `gh auth login` if you have not authenticated yet)"
            .to_owned();
    }
    if let Some(region) = ecr_region(registry) {
        return format!(
            "If this is a private repository, log in with the AWS CLI:\n    \
             aws ecr get-login-password --region {region} | \
             docker login --username AWS --password-stdin {registry}\n  \
             (or configure the `ecr-login` credential helper in your Docker \
             config; forge reads credentials from `~/.docker/config.json`)"
        );
    }
    if registry == "public.ecr.aws" {
        return format!(
            "Log in with the AWS CLI:\n    \
             aws ecr-public get-login-password --region us-east-1 | \
             docker login --username AWS --password-stdin {registry}\n  \
             (forge reads credentials from `~/.docker/config.json`)"
        );
    }
    format!(
        "If this registry requires authentication, log in with:\n    \
         docker login {registry}\n  \
         forge reads credentials from your Docker config \
         (`~/.docker/config.json`), including credential helpers."
    )
}

/// Region of a private Amazon ECR registry host —
/// `<account>.dkr.ecr.<region>.amazonaws.com` (also `ecr-fips` and the
/// `.com.cn` China partition). `None` for anything that doesn't match,
/// including ECR Public (`public.ecr.aws`).
fn ecr_region(registry: &str) -> Option<&str> {
    let rest = registry
        .split_once(".dkr.ecr.")
        .or_else(|| registry.split_once(".dkr.ecr-fips."))
        .map(|(_, rest)| rest)?;
    let region = rest
        .strip_suffix(".amazonaws.com")
        .or_else(|| rest.strip_suffix(".amazonaws.com.cn"))?;
    (!region.is_empty() && !region.contains('.')).then_some(region)
}

/// Honours `FORGE_OCI_INSECURE_HOSTS` (comma-separated `host[:port]`
/// list) to opt specific registries into plaintext HTTP. Default is
/// HTTPS for everything. Intended for local registries in tests/CI; do
/// not point this at production registries.
fn configured_protocol() -> ClientProtocol {
    match std::env::var("FORGE_OCI_INSECURE_HOSTS") {
        Ok(s) if !s.trim().is_empty() => {
            let list: Vec<String> = s.split(',').map(|x| x.trim().to_owned()).collect();
            ClientProtocol::HttpsExcept(list)
        }
        _ => ClientProtocol::Https,
    }
}

/// Pick the registry credentials for `registry`. For `ghcr.io` a GitHub
/// token (env or `gh` CLI) takes precedence so private GitHub packages
/// resolve without the user wiring up a separate token; for every
/// registry — ECR, Docker Hub, self-hosted, and `ghcr.io` when no GitHub
/// token is found — we fall through to the Docker credential store, the
/// same source `docker pull` uses.
fn resolve_auth(registry: &str) -> RegistryAuth {
    match auth_from_token(registry, ghcr_token(registry)) {
        RegistryAuth::Anonymous => docker_config_auth(registry),
        auth => auth,
    }
}

/// Credentials from the Docker credential store — see
/// [`crate::docker_auth`] for what is read and why. The pair goes over
/// HTTP Basic, which the registry exchanges for a bearer token per the
/// distribution spec (for ECR the pair is literally `AWS` / a token).
///
/// Nothing configured for this registry is the common case and degrades
/// silently to an anonymous pull. A *failure* — an unreadable config, a
/// credential helper that errored — is warned about rather than
/// propagated: the pull may still succeed anonymously if the plugin is
/// public, and if it doesn't, the access-denied hint tells the user how
/// to log in. Staying quiet here would leave them debugging a 401 with
/// no idea their `ecr-login` helper was failing.
fn docker_config_auth(registry: &str) -> RegistryAuth {
    match crate::docker_auth::credential(registry) {
        Ok(Some(credential)) => RegistryAuth::Basic(credential.username, credential.secret),
        Ok(None) => {
            tracing::debug!(
                target: "forge::oci",
                registry,
                "no Docker credential configured; pulling anonymously",
            );
            RegistryAuth::Anonymous
        }
        Err(e) => {
            tracing::warn!(
                target: "forge::oci",
                registry, error = %e,
                "could not obtain a Docker credential; pulling anonymously",
            );
            RegistryAuth::Anonymous
        }
    }
}

/// Pure mapping from `(registry, token)` to a `RegistryAuth`, factored
/// out of [`resolve_auth`] so the policy is unit-testable without a
/// real `gh` on `PATH`.
fn auth_from_token(registry: &str, token: Option<String>) -> RegistryAuth {
    match (registry, token) {
        (GHCR_REGISTRY, Some(token)) => RegistryAuth::Basic(GHCR_TOKEN_USERNAME.to_owned(), token),
        _ => RegistryAuth::Anonymous,
    }
}

/// Best-effort GitHub token for `ghcr.io`, in precedence order:
/// `GH_TOKEN`, then `GITHUB_TOKEN`, then `gh auth token`. The env vars
/// let CI authenticate without `gh` installed (and mirror `gh`'s own
/// precedence, so a local shell that overrides one keeps doing so).
/// Returns `None` — never an error — for non-GHCR registries or when no
/// source yields a token; auth is optional and a miss just means an
/// anonymous pull.
fn ghcr_token(registry: &str) -> Option<String> {
    if registry != GHCR_REGISTRY {
        return None;
    }
    token_from_env().or_else(token_from_gh_cli)
}

/// First non-empty value of `GH_TOKEN`, then `GITHUB_TOKEN`.
fn token_from_env() -> Option<String> {
    first_token([
        std::env::var("GH_TOKEN").ok(),
        std::env::var("GITHUB_TOKEN").ok(),
    ])
}

/// First candidate that is present and non-blank, trimmed. Pure helper
/// so the precedence rule is testable without touching process env.
fn first_token(candidates: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    candidates
        .into_iter()
        .flatten()
        .map(|t| t.trim().to_owned())
        .find(|t| !t.is_empty())
}

/// Token from `gh auth token`. `None` if `gh` is missing, the user is
/// not logged in, or the output is empty.
fn token_from_gh_cli() -> Option<String> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Does this pull error mean "the registry refused you," as opposed to
/// a transport/parse/not-found failure? Used to decide whether to attach
/// the ghcr.io auth hint. Covers both the bearer-exchange failure and a
/// `DENIED`/`UNAUTHORIZED` code in the registry's error envelope.
fn is_access_denied(err: &oci_client::errors::OciDistributionError) -> bool {
    use oci_client::errors::{OciDistributionError as E, OciErrorCode};
    match err {
        E::UnauthorizedError { .. } | E::AuthenticationFailure(_) => true,
        E::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|e| matches!(e.code, OciErrorCode::Denied | OciErrorCode::Unauthorized)),
        _ => false,
    }
}

/// Index of the layer to treat as the wasm plugin, given each layer's
/// media type in order. Prefers [`ACCEPTED_MEDIA_TYPES`] (first match
/// wins); otherwise, if the artifact has exactly one layer, accepts it
/// regardless — `oras push --artifact-type ...` defaults vary across
/// tools, and the wasmtime load step fails loudly on non-wasm bytes
/// anyway. Returns `None` when nothing qualifies.
///
/// Shared by [`pick_wasm_layer`] (the byte-bearing pull path) and
/// [`pick_wasm_descriptor`] (the manifest-only revalidation path) so both
/// agree on which layer is "the plugin" — otherwise a revalidated digest
/// could point at a different layer than the one we'd pull.
fn pick_wasm_index(media_types: &[&str]) -> Option<usize> {
    for accepted in ACCEPTED_MEDIA_TYPES {
        if let Some(i) = media_types.iter().position(|m| m == accepted) {
            return Some(i);
        }
    }
    (media_types.len() == 1).then_some(0)
}

fn pick_wasm_layer<'a>(
    layers: &'a [oci_client::client::ImageLayer],
    _manifest: &Option<OciImageManifest>,
    reference: &str,
) -> Result<&'a oci_client::client::ImageLayer, OciError> {
    let media_types: Vec<&str> = layers.iter().map(|l| l.media_type.as_str()).collect();
    match pick_wasm_index(&media_types) {
        Some(i) => Ok(&layers[i]),
        None => Err(OciError::NoWasmLayer {
            reference: reference.to_owned(),
            got: media_types.join(", "),
        }),
    }
}

/// Manifest-descriptor counterpart to [`pick_wasm_layer`], used by the
/// revalidation path where we have the layer *descriptors* (with digests)
/// but not their bytes.
fn pick_wasm_descriptor<'a>(
    layers: &'a [oci_client::manifest::OciDescriptor],
    reference: &str,
) -> Result<&'a oci_client::manifest::OciDescriptor, OciError> {
    let media_types: Vec<&str> = layers.iter().map(|l| l.media_type.as_str()).collect();
    match pick_wasm_index(&media_types) {
        Some(i) => Ok(&layers[i]),
        None => Err(OciError::NoWasmLayer {
            reference: reference.to_owned(),
            got: media_types.join(", "),
        }),
    }
}

// ── cache I/O ────────────────────────────────────────────────────────────

/// Base of forge's on-disk cache: `$FORGE_CACHE_DIR` if set, else the XDG
/// cache dir. The OCI plugin store and the compiled-component cache live in
/// sibling subdirectories under here.
pub(crate) fn cache_base() -> Result<PathBuf, OciError> {
    let base = if let Ok(env) = std::env::var("FORGE_CACHE_DIR") {
        PathBuf::from(env)
    } else {
        dirs::cache_dir().ok_or(OciError::NoCacheDir)?
    };
    Ok(base.join("openapi-forge"))
}

fn cache_root() -> Result<PathBuf, OciError> {
    Ok(cache_base()?.join("plugins"))
}

/// Directory for wasmtime's compiled-component cache. Sibling of the OCI
/// plugin store; wasmtime manages entries (eviction, versioning) within it.
pub(crate) fn compiled_cache_dir() -> Result<PathBuf, OciError> {
    Ok(cache_base()?.join("compiled"))
}

fn blob_path(cache_root: &Path, digest: &str) -> Result<PathBuf, OciError> {
    let (algo, hex) = digest
        .split_once(':')
        .ok_or_else(|| OciError::UnsupportedDigestAlgo(digest.to_owned()))?;
    if algo != "sha256" {
        return Err(OciError::UnsupportedDigestAlgo(digest.to_owned()));
    }
    Ok(cache_root
        .join("by-digest")
        .join(algo)
        .join(format!("{hex}.wasm")))
}

fn tag_pointer_path(cache_root: &Path, registry: &str, repository: &str, tag: &str) -> PathBuf {
    cache_root
        .join("by-tag")
        .join(sanitize(registry))
        .join(sanitize(repository))
        .join(format!("{}.digest", sanitize(tag)))
}

/// Strip path separators so registry/repo segments become single
/// directory names. Collisions are technically possible (e.g.
/// `foo/bar` vs `foo_bar`) but harmless: the canonical truth is the
/// blob store keyed by digest.
fn sanitize(s: &str) -> String {
    s.replace(['/', '\\'], "_")
}

fn read_blob_by_digest(cache_root: &Path, digest: &str) -> Result<Option<Vec<u8>>, OciError> {
    let path = blob_path(cache_root, digest)?;
    match std::fs::read(&path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(OciError::CacheIo { path, source }),
    }
}

fn write_blob(cache_root: &Path, digest: &str, bytes: &[u8]) -> Result<(), OciError> {
    let path = blob_path(cache_root, digest)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| OciError::CacheIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("wasm.tmp");
    std::fs::write(&tmp, bytes).map_err(|source| OciError::CacheIo {
        path: tmp.clone(),
        source,
    })?;
    std::fs::rename(&tmp, &path).map_err(|source| OciError::CacheIo {
        path: path.clone(),
        source,
    })
}

fn read_tag_pointer(
    cache_root: &Path,
    registry: &str,
    repository: &str,
    tag: &str,
) -> Result<Option<String>, OciError> {
    let path = tag_pointer_path(cache_root, registry, repository, tag);
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s.trim().to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(OciError::CacheIo { path, source }),
    }
}

fn write_tag_pointer(
    cache_root: &Path,
    registry: &str,
    repository: &str,
    tag: &str,
    digest: &str,
) -> Result<(), OciError> {
    let path = tag_pointer_path(cache_root, registry, repository, tag);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| OciError::CacheIo {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(&path, digest).map_err(|source| OciError::CacheIo {
        path: path.clone(),
        source,
    })
}

fn layer_digest(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{}", hex::encode(h.finalize()))
}

fn digests_equal(a: &str, b: &str) -> bool {
    // Normalise: both should be `algo:hex`. Compare case-insensitively
    // on the hex portion to be lenient about uppercase digests.
    let (a_algo, a_hex) = a.split_once(':').unwrap_or(("", a));
    let (b_algo, b_hex) = b.split_once(':').unwrap_or(("", b));
    a_algo == b_algo && a_hex.eq_ignore_ascii_case(b_hex)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use oci_client::manifest::{OciDescriptor, OciImageManifest};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serialises the tests that mutate process-global env
    /// (`FORGE_CACHE_DIR`, `FORGE_OCI_INSECURE_HOSTS`, `DOCKER_CONFIG`,
    /// `PATH`). Each mock server binds a random port, so the
    /// insecure-hosts override differs per test; without this lock the
    /// parallel test runner lets one test's `set_var` clobber the
    /// other's mid-fetch. Held only inside the `spawn_blocking` critical
    /// sections (never across `.await`). Shared with
    /// [`crate::docker_auth`]'s tests, which set `DOCKER_CONFIG` and
    /// `PATH` — one lock across both modules, since the env they race on
    /// is the same process-global.
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn parses_typical_ref() {
        let r: Reference = "ghcr.io/marcusdunn/typescript-fetch:0.1.0".parse().unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "marcusdunn/typescript-fetch");
        assert_eq!(r.tag(), Some("0.1.0"));
        assert_eq!(r.digest(), None);
    }

    #[test]
    fn parses_digest_pinned_ref() {
        let s =
            "ghcr.io/x/y@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let r: Reference = s.parse().unwrap();
        assert!(r.digest().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn blob_path_layout() {
        let root = PathBuf::from("/tmp/x");
        let p = blob_path(&root, "sha256:deadbeef").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/x/by-digest/sha256/deadbeef.wasm"));
    }

    #[test]
    fn rejects_non_sha256_digest() {
        let root = PathBuf::from("/tmp/x");
        assert!(matches!(
            blob_path(&root, "sha512:abc"),
            Err(OciError::UnsupportedDigestAlgo(_))
        ));
        assert!(matches!(
            blob_path(&root, "no-colon"),
            Err(OciError::UnsupportedDigestAlgo(_))
        ));
    }

    #[test]
    fn tag_pointer_layout_sanitises_slashes() {
        let root = PathBuf::from("/tmp/x");
        let p = tag_pointer_path(&root, "ghcr.io", "owner/repo", "1.0.0");
        assert_eq!(
            p,
            PathBuf::from("/tmp/x/by-tag/ghcr.io/owner_repo/1.0.0.digest")
        );
    }

    #[test]
    fn ghcr_token_maps_to_basic_auth() {
        let auth = auth_from_token("ghcr.io", Some("ghp_secret".to_owned()));
        assert_eq!(
            auth,
            RegistryAuth::Basic(GHCR_TOKEN_USERNAME.to_owned(), "ghp_secret".to_owned())
        );
    }

    #[test]
    fn ghcr_without_token_is_anonymous() {
        assert_eq!(auth_from_token("ghcr.io", None), RegistryAuth::Anonymous);
    }

    #[test]
    fn non_ghcr_registry_is_anonymous_even_with_token() {
        // We only ever surface a GitHub token for ghcr.io, but guard the
        // policy anyway: a token must never leak to a different registry.
        assert_eq!(
            auth_from_token("docker.io", Some("ghp_secret".to_owned())),
            RegistryAuth::Anonymous
        );
    }

    /// The ECR shape end-to-end: a `config.json` holding base64
    /// `AWS:<token>` (what `aws ecr get-login-password | docker login`
    /// leaves behind) must reach the registry as HTTP Basic.
    /// "QVdTOmVjci10b2tlbg==" is base64("AWS:ecr-token").
    #[test]
    fn docker_config_auths_entry_resolves_to_basic() {
        let env = crate::docker_auth::tests::EnvSandbox::new();
        env.write(
            "config.json",
            r#"{"auths": {"123456789012.dkr.ecr.us-east-1.amazonaws.com": {"auth": "QVdTOmVjci10b2tlbg=="}}}"#,
        );
        assert_eq!(
            resolve_auth("123456789012.dkr.ecr.us-east-1.amazonaws.com"),
            RegistryAuth::Basic("AWS".to_owned(), "ecr-token".to_owned())
        );
    }

    /// A podman-written `containers/auth.json` resolves the same way, so
    /// `podman-docker` machines authenticate without forge-specific
    /// setup. See issue #121.
    #[test]
    fn podman_auth_json_resolves_to_basic() {
        let env = crate::docker_auth::tests::EnvSandbox::new();
        env.write(
            "containers/auth.json",
            r#"{"auths": {"123456789012.dkr.ecr.us-east-1.amazonaws.com": {"auth": "QVdTOmVjci10b2tlbg=="}}}"#,
        );
        assert_eq!(
            resolve_auth("123456789012.dkr.ecr.us-east-1.amazonaws.com"),
            RegistryAuth::Basic("AWS".to_owned(), "ecr-token".to_owned())
        );
    }

    #[test]
    fn docker_config_without_entry_is_anonymous() {
        let env = crate::docker_auth::tests::EnvSandbox::new();
        env.write("config.json", r#"{"auths": {}}"#);
        assert_eq!(
            docker_config_auth("registry.example.com"),
            RegistryAuth::Anonymous
        );
    }

    /// A broken credential store must not fail the pull outright — it
    /// degrades to anonymous (with a warning), so public plugins keep
    /// working on a machine with a misconfigured Docker setup.
    #[test]
    fn unreadable_docker_config_degrades_to_anonymous() {
        let env = crate::docker_auth::tests::EnvSandbox::new();
        env.write("config.json", "{not json");
        assert_eq!(
            docker_config_auth("registry.example.com"),
            RegistryAuth::Anonymous
        );
    }

    /// A GitHub token must win over a stored Docker credential for
    /// ghcr.io — the `gh` login is the documented path and is refreshed
    /// far more often than a `docker login` against ghcr.
    #[test]
    fn ghcr_prefers_github_token_over_docker_config() {
        let env = crate::docker_auth::tests::EnvSandbox::new();
        env.write(
            "config.json",
            r#"{"auths": {"ghcr.io": {"auth": "dTpkb2NrZXI="}}}"#,
        );
        std::env::set_var("GH_TOKEN", "ghp_from_env");
        let auth = resolve_auth("ghcr.io");
        std::env::remove_var("GH_TOKEN");
        assert_eq!(
            auth,
            RegistryAuth::Basic(GHCR_TOKEN_USERNAME.to_owned(), "ghp_from_env".to_owned())
        );
    }

    #[test]
    fn ecr_region_parses_private_ecr_hosts() {
        assert_eq!(
            ecr_region("123456789012.dkr.ecr.us-east-1.amazonaws.com"),
            Some("us-east-1")
        );
        assert_eq!(
            ecr_region("123456789012.dkr.ecr-fips.us-gov-west-1.amazonaws.com"),
            Some("us-gov-west-1")
        );
        assert_eq!(
            ecr_region("123456789012.dkr.ecr.cn-north-1.amazonaws.com.cn"),
            Some("cn-north-1")
        );
        assert_eq!(ecr_region("public.ecr.aws"), None);
        assert_eq!(ecr_region("ghcr.io"), None);
        assert_eq!(
            ecr_region("evil.dkr.ecr.us-east-1.amazonaws.com.example"),
            None
        );
    }

    #[test]
    fn access_denied_hint_is_registry_specific() {
        assert!(access_denied_hint("ghcr.io").contains("read:packages"));
        assert!(
            access_denied_hint("123456789012.dkr.ecr.eu-west-2.amazonaws.com")
                .contains("aws ecr get-login-password --region eu-west-2")
        );
        assert!(access_denied_hint("public.ecr.aws").contains("aws ecr-public get-login-password"));
        assert!(access_denied_hint("registry.example.com")
            .contains("docker login registry.example.com"));
    }

    #[test]
    fn ghcr_token_skips_non_ghcr_registries() {
        // Must short-circuit before reading env or invoking `gh`.
        assert_eq!(ghcr_token("docker.io"), None);
    }

    #[test]
    fn first_token_picks_first_non_blank_trimmed() {
        // Empty / whitespace-only candidates are skipped; the winner is
        // trimmed. This is the GH_TOKEN-then-GITHUB_TOKEN precedence.
        assert_eq!(first_token([None, None]), None);
        assert_eq!(
            first_token([Some("".to_owned()), Some("  ".to_owned())]),
            None
        );
        assert_eq!(
            first_token([Some("  gh_a  ".to_owned()), Some("gh_b".to_owned())]),
            Some("gh_a".to_owned())
        );
        assert_eq!(
            first_token([Some("   ".to_owned()), Some("gh_b".to_owned())]),
            Some("gh_b".to_owned())
        );
    }

    #[test]
    fn denied_envelope_is_access_denied() {
        use oci_client::errors::{OciDistributionError, OciEnvelope, OciError, OciErrorCode};
        let err = OciDistributionError::RegistryError {
            url: "https://ghcr.io/v2/org/pkg/manifests/latest".to_owned(),
            envelope: OciEnvelope {
                errors: vec![OciError {
                    code: OciErrorCode::Denied,
                    message: "requested access to the resource is denied".to_owned(),
                    detail: serde_json::Value::Null,
                }],
            },
        };
        assert!(is_access_denied(&err));
    }

    #[test]
    fn unauthorized_error_is_access_denied() {
        use oci_client::errors::OciDistributionError;
        let err = OciDistributionError::UnauthorizedError {
            url: "https://ghcr.io/v2/org/pkg/manifests/latest".to_owned(),
        };
        assert!(is_access_denied(&err));
    }

    #[test]
    fn not_found_is_not_access_denied() {
        use oci_client::errors::OciDistributionError;
        // A genuine 404 / missing-manifest must not masquerade as an
        // auth problem — that would send users chasing a scope they
        // already have.
        let err = OciDistributionError::ImageManifestNotFoundError("nope".to_owned());
        assert!(!is_access_denied(&err));
    }

    #[test]
    fn digests_equal_normalises_case() {
        assert!(digests_equal("sha256:ABCDEF", "sha256:abcdef"));
        assert!(!digests_equal("sha256:abc", "sha512:abc"));
    }

    #[test]
    fn round_trip_blob_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let digest = layer_digest(b"hello");

        assert!(read_blob_by_digest(&root, &digest).unwrap().is_none());
        write_blob(&root, &digest, b"hello").unwrap();
        let got = read_blob_by_digest(&root, &digest).unwrap().unwrap();
        assert_eq!(got, b"hello");
    }

    #[test]
    fn round_trip_tag_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        assert!(read_tag_pointer(&root, "ghcr.io", "o/r", "1.0")
            .unwrap()
            .is_none());
        write_tag_pointer(&root, "ghcr.io", "o/r", "1.0", "sha256:deadbeef").unwrap();
        let got = read_tag_pointer(&root, "ghcr.io", "o/r", "1.0").unwrap();
        assert_eq!(got.as_deref(), Some("sha256:deadbeef"));
    }

    /// Stand up a wiremock server that speaks just enough of the OCI
    /// Distribution v2 protocol to satisfy `oci-client::Client::pull`,
    /// then drive `fetch_to_bytes` against it. Asserts:
    /// - the returned bytes match the layer payload byte-for-byte;
    /// - the cache is populated under the digest path on first run;
    /// - the second call still resolves to the same bytes after the
    ///   server is torn down: revalidation can't reach the registry, so
    ///   forge falls back to the cached blob for the tag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fetch_to_bytes_pulls_from_registry_and_caches() {
        let payload = b"\0asm\x01\x00\x00\x00 fake-wasm-payload-for-test".to_vec();
        let layer_dig = layer_digest(&payload);
        let config_blob = json!({}).to_string().into_bytes();
        let config_digest = layer_digest(&config_blob);

        let manifest = OciImageManifest {
            schema_version: 2,
            media_type: Some("application/vnd.oci.image.manifest.v1+json".to_string()),
            config: OciDescriptor {
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                digest: config_digest.clone(),
                size: config_blob.len() as i64,
                urls: None,
                annotations: None,
                artifact_type: None,
            },
            layers: vec![OciDescriptor {
                media_type: "application/wasm".to_string(),
                digest: layer_dig.clone(),
                size: payload.len() as i64,
                urls: None,
                annotations: None,
                artifact_type: None,
            }],
            subject: None,
            artifact_type: None,
            annotations: None,
        };
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        let manifest_digest = layer_digest(manifest_json.as_bytes());

        let server = MockServer::start().await;
        // Anonymous v2 ping — return 200 so the client skips the bearer flow.
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // Manifest by tag.
        Mock::given(method("GET"))
            .and(path("/v2/test/repo/manifests/v1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str())
                    .set_body_raw(
                        manifest_json.clone(),
                        "application/vnd.oci.image.manifest.v1+json",
                    ),
            )
            .mount(&server)
            .await;
        // Manifest by digest (the client may re-fetch by digest).
        Mock::given(method("GET"))
            .and(path(format!("/v2/test/repo/manifests/{manifest_digest}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
                    .set_body_raw(manifest_json, "application/vnd.oci.image.manifest.v1+json"),
            )
            .mount(&server)
            .await;
        // Config blob.
        Mock::given(method("GET"))
            .and(path(format!("/v2/test/repo/blobs/{config_digest}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(config_blob, "application/vnd.oci.image.config.v1+json"),
            )
            .mount(&server)
            .await;
        // Layer blob.
        Mock::given(method("GET"))
            .and(path(format!("/v2/test/repo/blobs/{layer_dig}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(payload.clone(), "application/wasm"),
            )
            .mount(&server)
            .await;

        let cache = tempfile::tempdir().unwrap();
        let host = server.address().to_string();
        let reference = format!("{host}/test/repo:v1");

        // The async test owns a multi-thread runtime; `fetch_to_bytes`
        // spins up its own current_thread runtime and `block_on`s on
        // it. That panics if called from within a runtime, so wrap in
        // spawn_blocking — this is also how the CLI binary will look
        // from the perspective of any future async caller.
        let cache_path = cache.path().to_path_buf();
        let reference2 = reference.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            // Takes ENV_LOCK and points every credential-file env var at
            // an empty temp dir, so the developer's real Docker/podman
            // logins can't reach this test.
            let _env = crate::docker_auth::tests::EnvSandbox::new();
            std::env::set_var("FORGE_CACHE_DIR", &cache_path);
            std::env::set_var("FORGE_OCI_INSECURE_HOSTS", &host);
            fetch_to_bytes(&reference2)
        })
        .await
        .unwrap()
        .expect("first fetch should succeed");
        assert_eq!(bytes, payload, "fetched bytes must equal layer payload");

        // Cache should now contain the blob keyed by digest and the
        // tag pointer file.
        let blob = blob_path(
            &cache.path().join("openapi-forge").join("plugins"),
            &layer_dig,
        )
        .unwrap();
        assert!(
            blob.exists(),
            "blob cache should exist at {}",
            blob.display()
        );

        // Second fetch — tear down the server. Revalidation will fail
        // with connection-refused; forge must fall back to the cached
        // blob for the tag and still return the right bytes.
        drop(server);
        let cache_path = cache.path().to_path_buf();
        let bytes2 = tokio::task::spawn_blocking(move || {
            let _env = crate::docker_auth::tests::EnvSandbox::new();
            std::env::set_var("FORGE_CACHE_DIR", &cache_path);
            fetch_to_bytes(&reference)
        })
        .await
        .unwrap()
        .expect("second fetch should fall back to the cached blob");
        assert_eq!(bytes2, payload);
    }

    /// Regression test for the stale mutable-tag bug (`:latest` re-push).
    ///
    /// Seed the cache as if a previous run had pulled `:latest` at an old
    /// build (a tag pointer plus its blob). Then stand up a registry whose
    /// `:latest` now resolves to a *different* layer digest.
    /// `fetch_to_bytes` must revalidate the tag against the registry and
    /// return the NEW bytes — not the stale cached blob. Before the
    /// revalidation fix this returned the stale blob without ever
    /// contacting the registry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tag_repush_is_revalidated_not_served_stale() {
        // The registry's *current* :latest points at this build.
        let new_payload = b"\0asm\x01\x00\x00\x00 new-build-after-tag-repush".to_vec();
        let layer_dig = layer_digest(&new_payload);
        let config_blob = json!({}).to_string().into_bytes();
        let config_digest = layer_digest(&config_blob);

        let manifest = OciImageManifest {
            schema_version: 2,
            media_type: Some("application/vnd.oci.image.manifest.v1+json".to_string()),
            config: OciDescriptor {
                media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                digest: config_digest.clone(),
                size: config_blob.len() as i64,
                urls: None,
                annotations: None,
                artifact_type: None,
            },
            layers: vec![OciDescriptor {
                media_type: "application/wasm".to_string(),
                digest: layer_dig.clone(),
                size: new_payload.len() as i64,
                urls: None,
                annotations: None,
                artifact_type: None,
            }],
            subject: None,
            artifact_type: None,
            annotations: None,
        };
        let manifest_json = serde_json::to_string(&manifest).unwrap();
        let manifest_digest = layer_digest(manifest_json.as_bytes());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        // Manifest by tag — hit by both the revalidation manifest fetch
        // and the subsequent full pull.
        Mock::given(method("GET"))
            .and(path("/v2/test/repo/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
                    .insert_header("Docker-Content-Digest", manifest_digest.as_str())
                    .set_body_raw(
                        manifest_json.clone(),
                        "application/vnd.oci.image.manifest.v1+json",
                    ),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/test/repo/manifests/{manifest_digest}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/vnd.oci.image.manifest.v1+json")
                    .set_body_raw(manifest_json, "application/vnd.oci.image.manifest.v1+json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/test/repo/blobs/{config_digest}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(config_blob, "application/vnd.oci.image.config.v1+json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/v2/test/repo/blobs/{layer_dig}")))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(new_payload.clone(), "application/wasm"),
            )
            .mount(&server)
            .await;

        let cache = tempfile::tempdir().unwrap();
        let cache_plugins = cache.path().join("openapi-forge").join("plugins");
        let host = server.address().to_string();
        let reference = format!("{host}/test/repo:latest");

        // Seed a STALE cache entry: a previous build's blob plus a tag
        // pointer for :latest pointing at it. This is exactly the state
        // that made the old code serve a stale plugin forever.
        let stale_payload = b"\0asm\x01\x00\x00\x00 STALE-build-must-not-be-served".to_vec();
        let stale_dig = layer_digest(&stale_payload);
        write_blob(&cache_plugins, &stale_dig, &stale_payload).unwrap();
        write_tag_pointer(&cache_plugins, &host, "test/repo", "latest", &stale_dig).unwrap();

        let cache_path = cache.path().to_path_buf();
        let host2 = host.clone();
        let reference2 = reference.clone();
        let bytes = tokio::task::spawn_blocking(move || {
            let _env = crate::docker_auth::tests::EnvSandbox::new();
            std::env::set_var("FORGE_CACHE_DIR", &cache_path);
            std::env::set_var("FORGE_OCI_INSECURE_HOSTS", &host2);
            fetch_to_bytes(&reference2)
        })
        .await
        .unwrap()
        .expect("fetch should revalidate the tag and pull the new build");

        assert_eq!(
            bytes, new_payload,
            "must serve the re-pushed build, not the stale cache"
        );
        assert_ne!(bytes, stale_payload, "the stale blob must not be returned");

        // The tag pointer should now track the new digest.
        let ptr = read_tag_pointer(&cache_plugins, &host, "test/repo", "latest").unwrap();
        assert_eq!(ptr.as_deref(), Some(layer_dig.as_str()));
    }
}
