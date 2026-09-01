//! Minimal reader for the Docker credential store, so plugin pulls work
//! against any authenticated OCI registry — Amazon ECR being the case
//! that motivated it.
//!
//! The contract we are implementing is "if `docker pull` works, `forge`
//! works," for the two ways an ECR credential reaches disk:
//!
//! 1. `aws ecr get-login-password | docker login --password-stdin …`,
//!    which writes a base64 `auth` entry (`AWS:<token>`) into
//!    `auths` in the config file. ECR tokens expire after 12 hours, so
//!    this is the "I logged in this morning" path.
//! 2. [`docker-credential-ecr-login`][helper], registered under
//!    `credHelpers` (or `credsStore`), which mints a fresh token on
//!    every invocation from ambient AWS credentials. This is the path
//!    that doesn't expire.
//!
//! [helper]: https://github.com/awslabs/amazon-ecr-credential-helper
//!
//! Both live in `$DOCKER_CONFIG/config.json` (default
//! `~/.docker/config.json`). We also read the containers-ecosystem
//! `auth.json` — `$REGISTRY_AUTH_FILE`, `$XDG_RUNTIME_DIR/containers/`,
//! `~/.config/containers/` — because on the many distros that ship
//! `podman-docker`, `docker` *is* podman: `docker login` succeeds and
//! writes there, so reading only Docker's own config breaks the promise
//! above in the one case it most needs to hold. Same `auths` schema, so
//! the same parser covers it. See [`config_paths`] for the order.
//!
//! This is deliberately *not* a general Docker-config implementation.
//! We read what those two paths produce and nothing more; notably the
//! `identitytoken` field and the `Username: "<token>"` helper convention
//! (bearer-token registries such as Azure ACR) are unhandled — such a
//! registry falls through to an anonymous pull and the caller's
//! access-denied hint, rather than being silently mis-authenticated.
//! Support them when someone actually needs them.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Deserialize;

/// A username/password pair for one registry. For ECR this is literally
/// `AWS` plus a short-lived token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credential {
    pub(crate) username: String,
    pub(crate) secret: String,
}

/// Something went wrong *after* we found configuration that was supposed
/// to yield a credential. "No config" and "no entry for this registry"
/// are not errors — they are `Ok(None)` — because pulling anonymously is
/// the correct behaviour for a public plugin on a machine that has never
/// run `docker login`.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DockerAuthError {
    #[error("reading Docker config at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing Docker config at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("Docker credential helper `{helper}` could not be run: {source}")]
    HelperSpawn {
        helper: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Docker credential helper `{helper}` failed: {message}")]
    HelperFailed { helper: String, message: String },
    #[error("Docker credential helper `{helper}` returned malformed output: {source}")]
    HelperOutput {
        helper: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("malformed `auth` entry for {registry} in Docker config: {reason}")]
    MalformedAuth { registry: String, reason: String },
}

/// Credentials for `registry` (a bare host, e.g. `ghcr.io` or
/// `123456789012.dkr.ecr.us-east-1.amazonaws.com`), or `None` when the
/// user has nothing configured for it.
///
/// Walks [`config_paths`] in order and returns the first credential
/// found. A file that is absent, or present but has no entry for this
/// registry, is skipped — otherwise a stray empty `~/.docker/config.json`
/// would mask a real podman login in `containers/auth.json`.
///
/// Errors do not abort the walk, since a broken Docker config should not
/// hide a working podman one. The first error is held back and returned
/// only if no later file yields a credential, so a genuine
/// misconfiguration still surfaces instead of silently pulling anonymous.
pub(crate) fn credential(registry: &str) -> Result<Option<Credential>, DockerAuthError> {
    let mut first_error = None;
    for path in config_paths() {
        match credential_from_path(&path, registry) {
            Ok(Some(credential)) => return Ok(Some(credential)),
            Ok(None) => {}
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// Read one credential file. `Ok(None)` if it does not exist or holds
/// nothing for `registry`.
fn credential_from_path(
    path: &Path,
    registry: &str,
) -> Result<Option<Credential>, DockerAuthError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DockerAuthError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let config: DockerConfig =
        serde_json::from_str(&raw).map_err(|source| DockerAuthError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    credential_from_config(&config, registry, run_helper)
}

/// Credential files to consult, most specific first:
///
/// 1. `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json` — what
///    real Docker writes.
/// 2. `$REGISTRY_AUTH_FILE` — the containers-ecosystem override, a file
///    path (not a directory, unlike `DOCKER_CONFIG`).
/// 3. `$XDG_RUNTIME_DIR/containers/auth.json` — where podman puts a
///    non-persistent login on Linux.
/// 4. `~/.config/containers/auth.json` — podman's persistent location,
///    and the usual one on macOS where `XDG_RUNTIME_DIR` is unset.
///
/// (2)–(4) exist because on machines where `docker` *is* podman
/// (`podman-docker` on NixOS, Debian, Fedora) a plain `docker login`
/// succeeds but writes `containers/auth.json`. Reading only Docker's own
/// config there means `docker pull` works while forge 401s — exactly the
/// "if `docker pull` works, `forge` works" promise this module exists to
/// keep. See issue #121.
///
/// Docker stays first so an explicit `DOCKER_CONFIG` or a real Docker
/// login keeps winning, unchanged from before podman support.
fn config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(dir) = std::env::var_os("DOCKER_CONFIG") {
        paths.push(Path::new(&dir).join("config.json"));
    } else if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".docker").join("config.json"));
    }

    if let Some(file) = std::env::var_os("REGISTRY_AUTH_FILE") {
        paths.push(PathBuf::from(file));
    }

    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        paths.push(Path::new(&dir).join("containers").join("auth.json"));
    }

    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".config").join("containers").join("auth.json"));
    }

    paths
}

/// The subset of `config.json` we understand.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DockerConfig {
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
    /// Per-registry credential helper suffix, e.g. `"ecr-login"` →
    /// `docker-credential-ecr-login`. Takes precedence over `auths`,
    /// matching Docker itself.
    #[serde(default)]
    cred_helpers: HashMap<String, String>,
    /// Fallback helper for registries with no more specific entry.
    creds_store: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthEntry {
    /// base64 of `username:password`. What `docker login` writes when no
    /// credential store is configured.
    auth: Option<String>,
    /// Some tools write these in the clear instead of (or alongside)
    /// `auth`.
    username: Option<String>,
    password: Option<String>,
}

/// Resolution order, mirroring Docker's: a registry-specific helper
/// wins, then a stored `auths` entry, then the catch-all `credsStore`.
/// `helper` is injected so the lookup is testable without installing a
/// real credential helper.
fn credential_from_config(
    config: &DockerConfig,
    registry: &str,
    helper: impl Fn(&str, &str) -> Result<Option<Credential>, DockerAuthError>,
) -> Result<Option<Credential>, DockerAuthError> {
    if let Some(name) = lookup(&config.cred_helpers, registry) {
        return helper(name, registry);
    }
    if let Some(entry) = lookup(&config.auths, registry) {
        if let Some(credential) = credential_from_entry(entry, registry)? {
            return Ok(Some(credential));
        }
    }
    match &config.creds_store {
        Some(name) if !name.is_empty() => helper(name, registry),
        _ => Ok(None),
    }
}

/// Find the entry for `registry`, tolerating the legacy URL-shaped keys
/// `docker login` still writes (`https://index.docker.io/v1/`). Exact
/// match first so the common case costs nothing.
fn lookup<'a, T>(map: &'a HashMap<String, T>, registry: &str) -> Option<&'a T> {
    map.get(registry).or_else(|| {
        map.iter()
            .find(|(k, _)| key_host(k) == registry)
            .map(|(_, v)| v)
    })
}

/// Host portion of an `auths`/`credHelpers` key: strips an `http(s)://`
/// scheme and any path after the host. A key with no scheme is returned
/// whole, since a bare registry may legitimately be `host:port`.
fn key_host(key: &str) -> &str {
    match key
        .strip_prefix("https://")
        .or_else(|| key.strip_prefix("http://"))
    {
        Some(rest) => rest.split('/').next().unwrap_or(rest),
        None => key,
    }
}

/// `auth` (base64 `user:pass`) if present, else a clear-text
/// username/password pair. `Ok(None)` when the entry carries neither —
/// e.g. a bare `{}` left behind by `docker logout`, which should fall
/// through to `credsStore` rather than fail the pull.
fn credential_from_entry(
    entry: &AuthEntry,
    registry: &str,
) -> Result<Option<Credential>, DockerAuthError> {
    if let Some(auth) = &entry.auth {
        return decode_auth(auth, registry).map(Some);
    }
    match (&entry.username, &entry.password) {
        (Some(username), Some(secret)) => Ok(Some(Credential {
            username: username.clone(),
            secret: secret.clone(),
        })),
        _ => Ok(None),
    }
}

/// Decode a base64 `username:password`. Padding is accepted but not
/// required — writers in the wild differ, and a credential that Docker
/// accepts should not fail here.
fn decode_auth(auth: &str, registry: &str) -> Result<Credential, DockerAuthError> {
    let malformed = |reason: &str| DockerAuthError::MalformedAuth {
        registry: registry.to_owned(),
        reason: reason.to_owned(),
    };
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let decoded = engine
        .decode(auth.trim())
        .map_err(|_| malformed("not valid base64"))?;
    let decoded = String::from_utf8(decoded).map_err(|_| malformed("not valid UTF-8"))?;
    let (username, secret) = decoded
        .split_once(':')
        .ok_or_else(|| malformed("expected `username:password`"))?;
    Ok(Credential {
        username: username.to_owned(),
        secret: secret.to_owned(),
    })
}

/// A credential helper reports "I have nothing for this registry" by
/// failing with this message. That is a normal miss, not a breakage.
const HELPER_NOT_FOUND: &str = "credentials not found";

/// Invoke `docker-credential-<name> get`, writing the registry to the
/// helper's stdin and reading a JSON credential from its stdout — the
/// protocol every Docker credential helper implements, `ecr-login`
/// included.
fn run_helper(name: &str, registry: &str) -> Result<Option<Credential>, DockerAuthError> {
    let binary = format!("docker-credential-{name}");
    let mut child = std::process::Command::new(&binary)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|source| DockerAuthError::HelperSpawn {
            helper: binary.clone(),
            source,
        })?;

    // Unwrap is sound: stdin was configured as a pipe just above. Drop
    // the handle so the helper sees EOF and proceeds.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let written = stdin
        .write_all(registry.as_bytes())
        .and_then(|()| stdin.flush());
    drop(stdin);
    // A helper that exits before reading its input gives us a broken
    // pipe; prefer its own diagnostics over that, so only surface the
    // write error if the process itself then looks healthy.
    let output = child
        .wait_with_output()
        .map_err(|source| DockerAuthError::HelperSpawn {
            helper: binary.clone(),
            source,
        })?;
    if let Err(source) = written {
        if output.status.success() {
            return Err(DockerAuthError::HelperSpawn {
                helper: binary,
                source,
            });
        }
    }

    if !output.status.success() {
        let message = helper_message(&output);
        if message.to_lowercase().contains(HELPER_NOT_FOUND) {
            return Ok(None);
        }
        return Err(DockerAuthError::HelperFailed {
            helper: binary,
            message,
        });
    }

    let reply: HelperReply =
        serde_json::from_slice(&output.stdout).map_err(|source| DockerAuthError::HelperOutput {
            helper: binary,
            source,
        })?;
    Ok(Some(Credential {
        username: reply.username,
        secret: reply.secret,
    }))
}

/// Best diagnostic a failed helper gave us: stderr if it wrote any,
/// else stdout (helpers commonly report a miss on stdout), else the
/// exit status.
fn helper_message(output: &std::process::Output) -> String {
    for stream in [&output.stderr, &output.stdout] {
        let text = String::from_utf8_lossy(stream);
        let text = text.trim();
        if !text.is_empty() {
            return text.to_owned();
        }
    }
    format!("exited with {}", output.status)
}

/// The helper's stdout. Field names are capitalised on the wire; the
/// `ServerURL` field it also returns is of no use to us.
#[derive(Debug, Deserialize)]
struct HelperReply {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn config(json: &str) -> DockerConfig {
        serde_json::from_str(json).unwrap()
    }

    /// Helper stub that fails the test if the lookup ever reaches it.
    fn unreachable_helper(
        name: &str,
        _registry: &str,
    ) -> Result<Option<Credential>, DockerAuthError> {
        panic!("credential helper `{name}` should not have been invoked");
    }

    const ECR: &str = "123456789012.dkr.ecr.us-east-1.amazonaws.com";

    #[test]
    fn decodes_auth_entry() {
        // base64("AWS:ecr-token") — the shape `aws ecr get-login-password
        // | docker login` leaves behind.
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{"auth": "QVdTOmVjci10b2tlbg=="}}}}}}"#
        ));
        let got = credential_from_config(&c, ECR, unreachable_helper).unwrap();
        assert_eq!(
            got,
            Some(Credential {
                username: "AWS".to_owned(),
                secret: "ecr-token".to_owned(),
            })
        );
    }

    #[test]
    fn secret_may_contain_colons() {
        // Split on the *first* colon only: ECR tokens are base64 blobs
        // that can themselves contain ':' once decoded.
        let got = decode_auth("QVdTOmE6Yjpj", ECR).unwrap(); // "AWS:a:b:c"
        assert_eq!(got.secret, "a:b:c");
    }

    #[test]
    fn accepts_unpadded_auth() {
        // base64("AWS:tok") without the trailing '='.
        let got = decode_auth("QVdTOnRvaw", ECR).unwrap();
        assert_eq!(got.username, "AWS");
        assert_eq!(got.secret, "tok");
    }

    #[test]
    fn clear_text_username_password_entry() {
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{"username": "AWS", "password": "tok"}}}}}}"#
        ));
        let got = credential_from_config(&c, ECR, unreachable_helper).unwrap();
        assert_eq!(got.unwrap().secret, "tok");
    }

    #[test]
    fn unrelated_registry_is_a_miss() {
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{"auth": "QVdTOmVjci10b2tlbg=="}}}}}}"#
        ));
        assert_eq!(
            credential_from_config(&c, "ghcr.io", unreachable_helper).unwrap(),
            None
        );
    }

    #[test]
    fn empty_config_is_a_miss() {
        let c = config("{}");
        assert_eq!(
            credential_from_config(&c, ECR, unreachable_helper).unwrap(),
            None
        );
    }

    #[test]
    fn unknown_config_fields_are_ignored() {
        // Real config.json files carry plenty we don't model
        // (`currentContext`, `plugins`, `experimental`, …). Parsing must
        // not fail on them.
        let c = config(&format!(
            r#"{{"currentContext": "desktop", "plugins": {{"x": {{}}}},
                 "auths": {{"{ECR}": {{"auth": "QVdTOmVjci10b2tlbg==", "email": "a@b.c"}}}}}}"#
        ));
        assert!(credential_from_config(&c, ECR, unreachable_helper)
            .unwrap()
            .is_some());
    }

    #[test]
    fn cred_helper_takes_precedence_over_auths() {
        // Docker resolves credHelpers first; a stale `auths` entry (an
        // expired ECR token) must not shadow the helper that would mint
        // a fresh one.
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{"auth": "QVdTOnN0YWxl"}}}},
                 "credHelpers": {{"{ECR}": "ecr-login"}}}}"#
        ));
        let got = credential_from_config(&c, ECR, |name, registry| {
            assert_eq!(name, "ecr-login");
            assert_eq!(registry, ECR);
            Ok(Some(Credential {
                username: "AWS".to_owned(),
                secret: "fresh".to_owned(),
            }))
        })
        .unwrap();
        assert_eq!(got.unwrap().secret, "fresh");
    }

    #[test]
    fn creds_store_is_the_fallback() {
        let c = config(r#"{"credsStore": "ecr-login"}"#);
        let got = credential_from_config(&c, ECR, |name, _| {
            assert_eq!(name, "ecr-login");
            Ok(Some(Credential {
                username: "AWS".to_owned(),
                secret: "from-store".to_owned(),
            }))
        })
        .unwrap();
        assert_eq!(got.unwrap().secret, "from-store");
    }

    #[test]
    fn logged_out_entry_falls_through_to_creds_store() {
        // `docker logout` can leave an empty object behind. That is not
        // a credential, and it must not stop us consulting the store.
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{}}}}, "credsStore": "ecr-login"}}"#
        ));
        let got = credential_from_config(&c, ECR, |_, _| {
            Ok(Some(Credential {
                username: "AWS".to_owned(),
                secret: "from-store".to_owned(),
            }))
        })
        .unwrap();
        assert_eq!(got.unwrap().secret, "from-store");
    }

    #[test]
    fn url_shaped_keys_match_a_bare_registry() {
        let c = config(r#"{"auths": {"https://index.docker.io/v1/": {"auth": "dTpw"}}}"#);
        let got = credential_from_config(&c, "index.docker.io", unreachable_helper).unwrap();
        assert_eq!(got.unwrap().username, "u");
    }

    #[test]
    fn key_host_strips_scheme_and_path_only() {
        assert_eq!(key_host("https://index.docker.io/v1/"), "index.docker.io");
        assert_eq!(
            key_host("http://registry.local:5000/x"),
            "registry.local:5000"
        );
        // No scheme: keep the whole key, since `host:port` is valid.
        assert_eq!(key_host("registry.local:5000"), "registry.local:5000");
        assert_eq!(key_host("ghcr.io"), "ghcr.io");
    }

    #[test]
    fn malformed_auth_is_an_error_not_a_silent_miss() {
        // The user configured *something*; failing loudly beats an
        // anonymous 401 they have to reverse-engineer.
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{"auth": "!!!not-base64"}}}}}}"#
        ));
        assert!(matches!(
            credential_from_config(&c, ECR, unreachable_helper),
            Err(DockerAuthError::MalformedAuth { .. })
        ));

        // base64("no-colon-here")
        let c = config(&format!(
            r#"{{"auths": {{"{ECR}": {{"auth": "bm8tY29sb24taGVyZQ=="}}}}}}"#
        ));
        assert!(matches!(
            credential_from_config(&c, ECR, unreachable_helper),
            Err(DockerAuthError::MalformedAuth { .. })
        ));
    }

    #[test]
    fn missing_helper_binary_is_an_error() {
        let err = run_helper("forge-nonexistent-helper", ECR).unwrap_err();
        assert!(matches!(err, DockerAuthError::HelperSpawn { .. }));
    }

    /// base64("AWS:<secret>") for the tests below, built without pulling
    /// base64 into dev-deps just to construct fixtures.
    fn aws_auth(secret: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(format!("AWS:{secret}"))
    }

    /// A one-registry credential file, in the `auths` schema shared by
    /// Docker's `config.json` and the containers `auth.json`.
    fn auth_file(registry: &str, secret: &str) -> String {
        format!(
            r#"{{"auths": {{"{registry}": {{"auth": "{}"}}}}}}"#,
            aws_auth(secret)
        )
    }

    /// Every env var that can point [`config_paths`] at a credential
    /// file. Tests must pin all of them: now that lookup falls through
    /// Docker's config into the podman locations, a test that pinned only
    /// `DOCKER_CONFIG` would go on to read the developer's real
    /// `~/.config/containers/auth.json` and pass or fail by accident.
    const CREDENTIAL_ENV: [&str; 4] = [
        "DOCKER_CONFIG",
        "REGISTRY_AUTH_FILE",
        "XDG_RUNTIME_DIR",
        "HOME",
    ];

    /// Holds `ENV_LOCK` and points every entry of [`CREDENTIAL_ENV`]
    /// inside one temp dir, restoring the real environment on drop.
    /// Layout, relative to the temp root:
    ///
    /// ```text
    /// config.json                    ← $DOCKER_CONFIG/config.json
    /// registry-auth.json             ← $REGISTRY_AUTH_FILE
    /// containers/auth.json           ← $XDG_RUNTIME_DIR/containers/auth.json
    /// .config/containers/auth.json   ← ~/.config/containers/auth.json
    /// ```
    pub(crate) struct EnvSandbox {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
        dir: tempfile::TempDir,
    }

    impl EnvSandbox {
        pub(crate) fn new() -> Self {
            let lock = crate::oci::tests::ENV_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let saved = CREDENTIAL_ENV
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect();
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::env::set_var("DOCKER_CONFIG", root);
            std::env::set_var("REGISTRY_AUTH_FILE", root.join("registry-auth.json"));
            std::env::set_var("XDG_RUNTIME_DIR", root);
            std::env::set_var("HOME", root);
            Self {
                _lock: lock,
                saved,
                dir,
            }
        }

        /// Write a credential file at `rel` under the sandbox root,
        /// creating parent directories.
        pub(crate) fn write(&self, rel: &str, contents: &str) {
            let path = self.dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
    }

    impl Drop for EnvSandbox {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn missing_config_file_is_a_miss() {
        let _env = EnvSandbox::new();
        assert_eq!(credential(ECR).unwrap(), None);
    }

    #[test]
    fn reads_config_from_docker_config_env() {
        let env = EnvSandbox::new();
        env.write("config.json", &auth_file(ECR, "ecr-token"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "ecr-token");
    }

    // ── podman / containers auth.json (issue #121) ───────────────────────

    #[test]
    fn reads_podman_auth_json_from_registry_auth_file() {
        let env = EnvSandbox::new();
        env.write("registry-auth.json", &auth_file(ECR, "from-authfile"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-authfile");
    }

    #[test]
    fn reads_podman_auth_json_from_xdg_runtime_dir() {
        let env = EnvSandbox::new();
        env.write("containers/auth.json", &auth_file(ECR, "from-xdg"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-xdg");
    }

    #[test]
    fn reads_podman_auth_json_from_home_config() {
        let env = EnvSandbox::new();
        env.write(".config/containers/auth.json", &auth_file(ECR, "from-home"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-home");
    }

    /// Docker's own config keeps winning, so adding podman support can't
    /// change what an existing Docker user resolves to.
    #[test]
    fn docker_config_takes_precedence_over_podman() {
        let env = EnvSandbox::new();
        env.write("config.json", &auth_file(ECR, "from-docker"));
        env.write("registry-auth.json", &auth_file(ECR, "from-authfile"));
        env.write("containers/auth.json", &auth_file(ECR, "from-xdg"));
        env.write(".config/containers/auth.json", &auth_file(ECR, "from-home"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-docker");
    }

    #[test]
    fn podman_sources_are_ordered_authfile_then_xdg_then_home() {
        let env = EnvSandbox::new();
        env.write("registry-auth.json", &auth_file(ECR, "from-authfile"));
        env.write("containers/auth.json", &auth_file(ECR, "from-xdg"));
        env.write(".config/containers/auth.json", &auth_file(ECR, "from-home"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-authfile");

        std::env::set_var("REGISTRY_AUTH_FILE", env.dir.path().join("gone.json"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-xdg");

        std::env::set_var("XDG_RUNTIME_DIR", env.dir.path().join("gone"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-home");
    }

    /// The exact shape reported in issue #121: on a `podman-docker` box a
    /// stray or logged-out `~/.docker/config.json` must not mask the
    /// podman login that `docker login` actually wrote.
    #[test]
    fn empty_docker_config_does_not_mask_podman_credential() {
        let env = EnvSandbox::new();
        env.write("config.json", r#"{"auths": {}}"#);
        env.write("containers/auth.json", &auth_file(ECR, "from-podman"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-podman");
    }

    /// A Docker config for a *different* registry must not shadow podman
    /// either — the walk is per-registry, not per-file.
    #[test]
    fn docker_config_for_other_registry_falls_through() {
        let env = EnvSandbox::new();
        env.write("config.json", &auth_file("ghcr.io", "ghcr-token"));
        env.write("containers/auth.json", &auth_file(ECR, "from-podman"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-podman");
    }

    /// A broken Docker config must not hide a working podman one; the
    /// error is held back in case nothing later matches.
    #[test]
    fn broken_docker_config_does_not_hide_podman_credential() {
        let env = EnvSandbox::new();
        env.write("config.json", "{not json");
        env.write("containers/auth.json", &auth_file(ECR, "from-podman"));
        assert_eq!(credential(ECR).unwrap().unwrap().secret, "from-podman");
    }

    /// ...but when nothing else yields a credential, that held-back error
    /// surfaces rather than silently degrading to an anonymous pull.
    #[test]
    fn broken_config_surfaces_when_nothing_else_matches() {
        let env = EnvSandbox::new();
        env.write("config.json", "{not json");
        assert!(matches!(
            credential(ECR),
            Err(DockerAuthError::Parse { .. })
        ));
    }

    /// End-to-end over the real subprocess protocol, using a shell script
    /// standing in for `docker-credential-ecr-login`: it must receive the
    /// registry on stdin and have its stdout JSON parsed.
    #[cfg(unix)]
    #[test]
    fn helper_subprocess_round_trip() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("docker-credential-forge-test");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             read -r server\n\
             printf '{\"ServerURL\":\"%s\",\"Username\":\"AWS\",\"Secret\":\"tok-%s\"}' \
               \"$server\" \"$server\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _env = crate::oci::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", tmp.path().display()));
        let got = run_helper("forge-test", ECR);
        std::env::set_var("PATH", old_path);

        assert_eq!(
            got.unwrap(),
            Some(Credential {
                username: "AWS".to_owned(),
                secret: format!("tok-{ECR}"),
            })
        );
    }

    /// A helper that reports a miss the conventional way — non-zero exit
    /// with "credentials not found …" — is a miss, not an error. This is
    /// what `ecr-login` does for a registry outside your AWS account, and
    /// it must degrade to an anonymous pull.
    #[cfg(unix)]
    #[test]
    fn helper_reporting_not_found_is_a_miss() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("docker-credential-forge-missing");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             echo 'credentials not found in native keychain' >&2\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _env = crate::oci::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", tmp.path().display()));
        let got = run_helper("forge-missing", ECR);
        std::env::set_var("PATH", old_path);

        assert_eq!(got.unwrap(), None);
    }

    /// A helper that fails for any *other* reason (expired AWS creds,
    /// misconfiguration) must surface, so the user sees why rather than
    /// a bare anonymous 401.
    #[cfg(unix)]
    #[test]
    fn helper_failing_for_another_reason_is_an_error() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("docker-credential-forge-broken");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho 'ExpiredToken: security token expired' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _env = crate::oci::tests::ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", tmp.path().display()));
        let got = run_helper("forge-broken", ECR);
        std::env::set_var("PATH", old_path);

        let err = got.unwrap_err();
        assert!(
            matches!(&err, DockerAuthError::HelperFailed { message, .. }
                     if message.contains("ExpiredToken")),
            "unexpected error: {err}"
        );
    }
}
