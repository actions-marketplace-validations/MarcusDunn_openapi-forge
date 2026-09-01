//! Shared mutable state passed through the walkers.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use forge_ir::{
    ApiInfo, Diagnostic, NamedType, Operation, SecurityRequirement, SecurityScheme, Server, Webhook,
};
use indexmap::IndexMap;

#[cfg(test)]
use crate::external::NoExternalResolver;
use crate::external::Resolver;
use crate::refs::RefIndex;
use crate::value::ValuePool;

/// Synthetic canonical path used as the "main spec" key when the parser
/// is invoked via `parse_str` (no real file). Doc paths in the cycle set
/// and ref-index map use this key for the in-memory spec; relative
/// `$ref`s would fail to resolve since `NoExternalResolver` rejects them.
pub(crate) fn synthetic_main_path() -> PathBuf {
    PathBuf::from("<spec>")
}

#[derive(Debug)]
pub(crate) struct Ctx<'a> {
    pub file: Option<&'a str>,
    pub diagnostics: Vec<Diagnostic>,
    /// Type pool. Insertion preserves order; finalize topo-sorts before
    /// emitting the IR.
    pub types: IndexMap<String, NamedType>,
    pub operations: Vec<Operation>,
    /// 3.1+ inbound webhooks. Same `Operation` shape as `operations`,
    /// just lives on a separate IR list so generators can ignore them
    /// without filtering.
    pub webhooks: Vec<Webhook>,
    pub servers: Vec<Server>,
    pub info: Option<ApiInfo>,
    /// Per-document component-schema indices, keyed by canonical path.
    /// The main spec sits at `synthetic_main_path()` (or its real
    /// canonical path when `parse_path` is used).
    pub doc_refs: HashMap<PathBuf, RefIndex>,
    /// Cached root values for every loaded external document. Lets the
    /// schema walker resolve fragment-only refs (`#/Owner`) inside an
    /// already-loaded doc without going back through the resolver.
    pub doc_roots: HashMap<PathBuf, std::sync::Arc<serde_json::Value>>,
    /// Doc-stem prefix per loaded external document. The main spec maps
    /// to an empty string (no prefix).
    pub doc_prefix: HashMap<PathBuf, String>,
    /// The document the schema walker is currently resolving relative
    /// refs against. Switched on every external `$ref` traversal.
    pub current_doc: PathBuf,
    /// In-flight `(document, schema_id)` pairs, preventing the schema
    /// walker from infinitely recursing on cyclic refs. Re-entry into a
    /// pair already present returns the pre-registered id immediately.
    pub walking: HashSet<(PathBuf, String)>,
    /// Map from `(canonical_path, json_pointer_fragment)` → final type id
    /// for every schema that has been walked via an external `$ref`. The
    /// same target schema can be reached more than once with different
    /// `NameHint`s — `Named("Pet")` from `components.schemas.Pet` produces
    /// id `Pet`, and a later `Inline { .. }` hint from a response-content
    /// or array-items ref would otherwise produce a fresh `<docprefix>Pet`
    /// id and re-walk the schema, duplicating it in the type pool.
    /// Recording the mapping after the first walk lets subsequent
    /// resolutions short-circuit to the existing id.
    pub external_ref_to_id: HashMap<(PathBuf, String), String>,
    /// Names from the main spec's `components.pathItems` that have been
    /// `$ref`'d during paths/webhooks/callbacks walking. Used to surface
    /// unused-declaration warnings at the end of parse.
    pub referenced_component_path_items: HashSet<String>,
    /// Names from the main spec's 3.2 `components.mediaTypes` that have
    /// been `$ref`'d during operation/body walking. Same warning policy
    /// as `pathItems`.
    pub referenced_component_media_types: HashSet<String>,
    /// `true` when the root document declares `openapi: 3.0.x`. OAS 3.0
    /// forbade siblings on `$ref`; 3.1+ inherits JSON Schema 2020-12's
    /// allowance. The schema walker uses this to pick the right
    /// diagnostic.
    pub is_oas_3_0: bool,
    /// Bumps suffixes for inline-type id collisions.
    pub ident_counter: HashMap<String, u32>,
    pub security_schemes: Vec<SecurityScheme>,
    /// Top-level `security` array; operations without their own `security`
    /// inherit this list.
    pub default_security: Vec<SecurityRequirement>,
    /// External-document loader. The default `parse_str` entry uses
    /// `NoExternalResolver`; `parse_path` swaps in a `FileResolver`.
    pub resolver: Box<dyn Resolver>,
    /// Value pool. Every `Value` referenced from the IR (defaults,
    /// examples, link parameters, extensions, constraint bounds) is
    /// interned here. Finalised into [`forge_ir::Ir::values`].
    pub values: ValuePool,
}

impl<'a> Ctx<'a> {
    #[cfg(test)]
    pub fn new(file: Option<&'a str>) -> Self {
        Self::with_resolver(file, Box::new(NoExternalResolver), synthetic_main_path())
    }

    pub fn with_resolver(
        file: Option<&'a str>,
        resolver: Box<dyn Resolver>,
        main_doc: PathBuf,
    ) -> Self {
        let mut doc_refs = HashMap::new();
        doc_refs.insert(main_doc.clone(), RefIndex::default());
        let mut doc_prefix = HashMap::new();
        doc_prefix.insert(main_doc.clone(), String::new());
        Self {
            file,
            diagnostics: Vec::new(),
            types: IndexMap::new(),
            operations: Vec::new(),
            webhooks: Vec::new(),
            servers: Vec::new(),
            info: None,
            doc_refs,
            doc_roots: HashMap::new(),
            doc_prefix,
            current_doc: main_doc,
            walking: HashSet::new(),
            external_ref_to_id: HashMap::new(),
            referenced_component_path_items: HashSet::new(),
            referenced_component_media_types: HashSet::new(),
            is_oas_3_0: false,
            ident_counter: HashMap::new(),
            security_schemes: Vec::new(),
            default_security: Vec::new(),
            resolver,
            values: ValuePool::new(),
        }
    }

    pub fn refs(&self) -> &RefIndex {
        self.doc_refs
            .get(&self.current_doc)
            .expect("current_doc must always have an entry")
    }

    pub fn refs_mut(&mut self) -> &mut RefIndex {
        let key = self.current_doc.clone();
        self.doc_refs.entry(key).or_default()
    }

    /// Reserve a unique id for an inline type. Repeated calls with the same
    /// hint return distinct ids: `foo`, `foo_2`, `foo_3`, ...
    pub fn unique_id(&mut self, base: &str) -> String {
        if !self.types.contains_key(base) {
            self.ident_counter.insert(base.to_string(), 1);
            return base.to_string();
        }
        let counter = self.ident_counter.entry(base.to_string()).or_insert(1);
        loop {
            *counter += 1;
            let candidate = format!("{base}_{}", *counter);
            if !self.types.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    pub fn push_type(&mut self, t: NamedType) {
        self.types.insert(t.id.clone(), t);
    }

    pub fn push_diag(&mut self, d: Diagnostic) {
        self.diagnostics.push(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_ir::{AdditionalProperties, ObjectConstraints, ObjectType, TypeDef};

    fn empty_obj(id: &str) -> NamedType {
        NamedType {
            id: id.to_string(),
            original_name: None,
            title: None,
            description: None,
            deprecated: false,
            read_only: false,
            write_only: false,
            external_docs: None,
            default: None,
            examples: vec![],
            xml: None,
            definition: TypeDef::Object(ObjectType {
                properties: vec![],
                pattern_properties: vec![],
                additional_properties: AdditionalProperties::Forbidden,
                property_names: None,
                constraints: ObjectConstraints::default(),
            }),
            extensions: vec![],
            location: None,
        }
    }

    #[test]
    fn unique_id_no_collision() {
        let mut c = Ctx::new(None);
        assert_eq!(c.unique_id("Foo"), "Foo");
    }

    #[test]
    fn unique_id_collision_bumps() {
        let mut c = Ctx::new(None);
        c.push_type(empty_obj("Foo"));
        assert_eq!(c.unique_id("Foo"), "Foo_2");
        c.push_type(empty_obj("Foo_2"));
        assert_eq!(c.unique_id("Foo"), "Foo_3");
    }
}
