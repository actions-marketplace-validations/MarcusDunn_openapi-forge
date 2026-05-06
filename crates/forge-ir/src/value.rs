//! Structured value used for defaults, examples, extensions, link
//! parameters, and constraint values.
//!
//! Plugins do not link a JSON parser to read these — the host populates
//! them from the parsed spec.
//!
//! # Pool design (issue #85 follow-up, ADR-0007 amendment realised)
//!
//! WIT does not support recursive variants. `list<value>` and
//! `object(list<tuple<string, value>>)` cannot be represented at the
//! boundary directly, so the compound arms hold [`ValueRef`] indices
//! into [`crate::Ir::values`] rather than recursing by value. Every
//! `Value`-shaped IR field stores a `ValueRef`; the pool is the only
//! place that owns a `Value`.
//!
//! - `Value::List { items }` — `items: Vec<ValueRef>`
//! - `Value::Object { fields }` — `fields: Vec<(String, ValueRef)>`
//!
//! The pool is structurally deduplicated by the parser: pushing a `Value`
//! that is already present at index `i` returns `i`. SDK helpers
//! (`resolve`, `to_json`) walk the pool to materialise tree-shaped
//! representations on demand.
//!
//! See ADR-0006 for the parallel type-pool design.

use serde::{Deserialize, Serialize};

/// Index into [`crate::Ir::values`].
pub type ValueRef = u32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Value {
    Null,
    Bool {
        value: bool,
    },
    Int {
        value: i64,
    },
    Float {
        value: f64,
    },
    String {
        value: String,
    },
    /// JSON array, stored as a list of pool indices.
    List {
        items: Vec<ValueRef>,
    },
    /// JSON object, stored as a list of `(key, pool-index)` pairs in
    /// declared order.
    Object {
        fields: Vec<(String, ValueRef)>,
    },
}

impl Value {
    /// Convenience constructor for a string literal.
    pub fn s(s: impl Into<String>) -> Self {
        Value::String { value: s.into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_scalars() {
        for v in [
            Value::Null,
            Value::Bool { value: true },
            Value::Int { value: 42 },
            Value::Float { value: 1.5 },
            Value::s("hello"),
        ] {
            let s = serde_json::to_string(&v).unwrap();
            let back: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(v, back);
        }
    }

    #[test]
    fn json_roundtrip_compound() {
        let list = Value::List {
            items: vec![0, 1, 2],
        };
        let obj = Value::Object {
            fields: vec![("a".into(), 0), ("b".into(), 1)],
        };
        for v in [list, obj] {
            let s = serde_json::to_string(&v).unwrap();
            let back: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(v, back);
        }
    }
}
