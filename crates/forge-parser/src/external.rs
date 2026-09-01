//! External `$ref` resolution.
//!
//! The parser's default `parse_str` path uses [`NoExternalResolver`],
//! which rejects every external ref with `parser/E-EXTERNAL-REF`. The
//! file-based [`parse_path`](crate::parse_path) entry installs a
//! [`FileResolver`] that loads adjacent JSON documents (caching by
//! canonical path) and refuses any path that escapes the input file's
//! parent directory.
//!
//! The schema walker calls [`Resolver::load`] when it encounters a `$ref`
//! whose path part is non-empty; it then walks the target component just
//! like a local one, prefixed with the external document's stem so IDs
//! stay globally unique inside the type pool.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

#[derive(Debug)]
pub struct LoadedDoc {
    pub canonical_path: PathBuf,
    /// Shared handle to the loaded document. Reference-counted so repeated
    /// loads of the same logical document are a refcount bump, not a deep
    /// clone of the whole (potentially multi-hundred-KB) JSON tree.
    pub root: Arc<Value>,
}

#[derive(Debug)]
pub enum ResolverError {
    /// The current resolver does not handle external `$ref`s. The default
    /// `parse_str` entry uses a no-op resolver; users who want external
    /// refs should call `parse_path`.
    NotConfigured { raw: String },
    /// URL refs (`http://`, `https://`, ...) — deferred behind a
    /// follow-up issue.
    UrlNotSupported { raw: String },
    /// The path canonicalised outside the allowed root.
    EscapesRoot { attempted: PathBuf, root: PathBuf },
    /// Filesystem error during load.
    Io { path: PathBuf, message: String },
    /// JSON parse error from the loaded file.
    InvalidJson { path: PathBuf, message: String },
}

impl fmt::Display for ResolverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolverError::NotConfigured { raw } => write!(
                f,
                "external `$ref` `{raw}` requires a file-based resolver; \
                 call `parse_path` instead of `parse_str`"
            ),
            ResolverError::UrlNotSupported { raw } => write!(
                f,
                "URL `$ref` `{raw}` is not yet supported (file-relative refs only)"
            ),
            ResolverError::EscapesRoot { attempted, root } => write!(
                f,
                "external `$ref` resolves to `{}`, which is outside the input file's directory `{}`",
                attempted.display(),
                root.display()
            ),
            ResolverError::Io { path, message } => {
                write!(f, "failed to read `{}`: {message}", path.display())
            }
            ResolverError::InvalidJson { path, message } => {
                write!(f, "failed to parse `{}`: {message}", path.display())
            }
        }
    }
}

pub trait Resolver: fmt::Debug + Send {
    /// Load the document referenced by `raw_ref`, resolved relative to
    /// `current_doc`. Implementations cache by canonical path; repeated
    /// loads of the same logical document return a shared `Arc` handle to
    /// the same `Value` without re-reading or re-cloning the JSON tree.
    fn load(&mut self, raw_ref: &str, current_doc: &Path) -> Result<LoadedDoc, ResolverError>;
}

#[derive(Debug, Default)]
pub struct NoExternalResolver;

impl Resolver for NoExternalResolver {
    fn load(&mut self, raw_ref: &str, _: &Path) -> Result<LoadedDoc, ResolverError> {
        Err(ResolverError::NotConfigured {
            raw: raw_ref.to_string(),
        })
    }
}

#[derive(Debug)]
pub struct FileResolver {
    /// Canonical path to the directory enclosing the input spec. Every
    /// loaded path must canonicalise under this root.
    root: PathBuf,
    cache: HashMap<PathBuf, Arc<Value>>,
}

impl FileResolver {
    pub fn new(spec_path: &Path) -> std::io::Result<Self> {
        let canonical = spec_path.canonicalize()?;
        let root = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(canonical);
        Ok(Self {
            root,
            cache: HashMap::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Resolver for FileResolver {
    fn load(&mut self, raw_ref: &str, current_doc: &Path) -> Result<LoadedDoc, ResolverError> {
        let (file_part, _fragment) = split_ref(raw_ref);
        if is_url(file_part) {
            return Err(ResolverError::UrlNotSupported {
                raw: raw_ref.to_string(),
            });
        }
        let base = current_doc.parent().unwrap_or(current_doc);
        let candidate = base.join(file_part);
        let canonical = candidate.canonicalize().map_err(|e| ResolverError::Io {
            path: candidate.clone(),
            message: e.to_string(),
        })?;
        if !canonical.starts_with(&self.root) {
            return Err(ResolverError::EscapesRoot {
                attempted: canonical,
                root: self.root.clone(),
            });
        }
        if let Some(cached) = self.cache.get(&canonical) {
            return Ok(LoadedDoc {
                canonical_path: canonical,
                root: Arc::clone(cached),
            });
        }
        let text = std::fs::read_to_string(&canonical).map_err(|e| ResolverError::Io {
            path: canonical.clone(),
            message: e.to_string(),
        })?;
        let value: Value = serde_json::from_str(&text).map_err(|e| ResolverError::InvalidJson {
            path: canonical.clone(),
            message: e.to_string(),
        })?;
        let value = Arc::new(value);
        self.cache.insert(canonical.clone(), Arc::clone(&value));
        Ok(LoadedDoc {
            canonical_path: canonical,
            root: value,
        })
    }
}

/// Split a `$ref` string into `(path_part, fragment_without_hash)`.
pub(crate) fn split_ref(raw: &str) -> (&str, &str) {
    match raw.find('#') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, ""),
    }
}

pub(crate) fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("ftp://")
}

/// Walk an RFC 6901 JSON pointer fragment against `root`. Returns the
/// pointed-at value, or `None` if any token doesn't match. The fragment
/// is expected without its leading `#`; an empty fragment addresses the
/// root.
pub(crate) fn resolve_pointer<'a>(root: &'a Value, fragment: &str) -> Option<&'a Value> {
    if fragment.is_empty() {
        return Some(root);
    }
    let trimmed = fragment.strip_prefix('/').unwrap_or(fragment);
    if trimmed.is_empty() {
        return Some(root);
    }
    let mut cur = root;
    for token in trimmed.split('/') {
        let decoded = decode_pointer_token(token);
        cur = match cur {
            Value::Object(map) => map.get(&decoded)?,
            Value::Array(items) => {
                let idx: usize = decoded.parse().ok()?;
                items.get(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

/// Decode a single RFC 6901 token: `~1` → `/`, `~0` → `~`. Order
/// matters: `~1` before `~0` so a literal `~01` decodes correctly.
fn decode_pointer_token(s: &str) -> String {
    s.replace("~1", "/").replace("~0", "~")
}

/// Last token of a JSON pointer fragment. Used to derive a schema name
/// from `/AIAgent` or `/components/schemas/Pet`.
pub(crate) fn fragment_last_token(fragment: &str) -> Option<String> {
    let trimmed = fragment.strip_prefix('/').unwrap_or(fragment);
    if trimmed.is_empty() {
        return None;
    }
    trimmed.rsplit('/').next().map(decode_pointer_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_pointer_root() {
        let v = json!({"a": 1});
        assert_eq!(resolve_pointer(&v, ""), Some(&v));
        assert_eq!(resolve_pointer(&v, "/"), Some(&v));
    }

    #[test]
    fn resolve_pointer_walks_objects() {
        let v = json!({"components": {"schemas": {"Pet": {"type": "object"}}}});
        let pet = resolve_pointer(&v, "/components/schemas/Pet").unwrap();
        assert_eq!(pet["type"], "object");
    }

    #[test]
    fn resolve_pointer_flat_root_schema() {
        let v = json!({"AIAgent": {"type": "object"}});
        let agent = resolve_pointer(&v, "/AIAgent").unwrap();
        assert_eq!(agent["type"], "object");
    }

    #[test]
    fn resolve_pointer_decodes_escape() {
        let v = json!({"/api/v1/users": {"get": {}}});
        let item = resolve_pointer(&v, "/~1api~1v1~1users").unwrap();
        assert!(item.get("get").is_some());
    }

    #[test]
    fn resolve_pointer_walks_arrays() {
        let v = json!({"items": [10, 20, 30]});
        let item = resolve_pointer(&v, "/items/1").unwrap();
        assert_eq!(item, &json!(20));
    }

    #[test]
    fn fragment_last_token_works() {
        assert_eq!(
            fragment_last_token("/components/schemas/Pet"),
            Some("Pet".to_string())
        );
        assert_eq!(fragment_last_token("/AIAgent"), Some("AIAgent".to_string()));
        assert_eq!(
            fragment_last_token("/~1api~1v1~1users"),
            Some("/api/v1/users".to_string())
        );
        assert_eq!(fragment_last_token(""), None);
    }
}
