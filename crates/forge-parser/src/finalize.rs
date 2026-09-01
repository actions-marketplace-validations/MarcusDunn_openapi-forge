//! Finalize the parsed IR: sort operations by id, topologically sort
//! types (with cycles permitted as recursion groups), validate
//! determinism invariants.

use std::collections::{BTreeMap, HashMap, HashSet};

use forge_ir::{AdditionalProperties, Diagnostic, Ir, NamedType, Severity, TypeDef, TypeRef};

/// Canonicalize the IR. Mutates `ir` in place. Returned diagnostics
/// describe inconsistencies the parser produced (programmer error in the
/// parser). Cycles are no longer fatal — recursive component schemas are
/// emitted intact; downstream plugins reject via `StageError::Rejected`
/// if they can't handle them.
pub(crate) fn canonicalize(ir: &mut Ir) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // 1. Sort operations by id (stable).
    ir.operations.sort_by(|a, b| a.id.cmp(&b.id));

    // 2. Validate every TypeRef resolves (collect dangling refs into diags
    //    rather than panicking).
    let known: HashSet<String> = ir.types.iter().map(|t| t.id.clone()).collect();
    for t in &ir.types {
        for r in refs_of(t) {
            if !known.contains(&r) {
                diags.push(Diagnostic {
                    severity: Severity::Error,
                    code: crate::diag::E_DANGLING_REF.into(),
                    message: format!("type `{}` references unknown type `{}`", t.id, r),
                    location: t.location.clone(),
                    related: vec![],
                    suggested_fix: None,
                });
            }
        }
    }

    // 3. Topologically sort types via SCC + DAG topo. Recursive groups
    //    (multi-node SCCs and self-loops) emit alphabetically together at
    //    their topo position. An info-severity `W-RECURSIVE-TYPE`
    //    diagnostic per group helps with debugging plugin compatibility.
    let (sorted, scc_groups) = topo_sort_with_sccs(&ir.types);
    ir.types = sorted;
    for group in scc_groups {
        diags.push(Diagnostic {
            severity: Severity::Info,
            code: crate::diag::W_RECURSIVE_TYPE.into(),
            message: format!(
                "recursive type group emitted as a unit: [{}]",
                group.join(", ")
            ),
            location: None,
            related: vec![],
            suggested_fix: None,
        });
    }

    diags
}

/// Direct outgoing TypeRef edges from a NamedType.
fn refs_of(t: &NamedType) -> Vec<TypeRef> {
    let mut out = Vec::new();
    match &t.definition {
        TypeDef::Primitive(_)
        | TypeDef::EnumString(_)
        | TypeDef::EnumInt(_)
        | TypeDef::EnumBool(_)
        | TypeDef::EnumNumber(_)
        | TypeDef::Null
        | TypeDef::Any => {}
        TypeDef::Object(o) => {
            for p in &o.properties {
                out.push(p.r#type.clone());
            }
            if let AdditionalProperties::Typed { r#type } = &o.additional_properties {
                out.push(r#type.clone());
            }
        }
        TypeDef::Array(a) => out.push(a.items.clone()),
        TypeDef::Union(u) => {
            for v in &u.variants {
                out.push(v.r#type.clone());
            }
        }
    }
    out
}

/// Topo-sort with cycle support. Returns the ordered type list plus a list
/// of SCCs that contained more than one node (or a single node with a
/// self-loop) — the caller can surface those as recursion groups.
fn topo_sort_with_sccs(types: &[NamedType]) -> (Vec<NamedType>, Vec<Vec<String>>) {
    let by_id: HashMap<String, &NamedType> = types.iter().map(|t| (t.id.clone(), t)).collect();
    let known: HashSet<String> = by_id.keys().cloned().collect();

    // 1. Compute SCCs via Tarjan's algorithm.
    let sccs = tarjan_sccs(types, &by_id, &known);

    // 2. Assign each node to its SCC index.
    let mut scc_of: HashMap<String, usize> = HashMap::new();
    for (i, scc) in sccs.iter().enumerate() {
        for m in scc {
            scc_of.insert(m.clone(), i);
        }
    }

    // Each SCC's representative for tiebreaking is its alphabetically
    // smallest member.
    let scc_rep: Vec<String> = sccs
        .iter()
        .map(|scc| scc.iter().min().cloned().unwrap_or_default())
        .collect();

    // 3. Build the SCC-DAG. Edge `from -> to` means types in `from` depend
    //    on types in `to`, so `from` must come AFTER `to` (Kahn: indeg of
    //    `from` increments).
    let mut indeg: BTreeMap<usize, usize> = (0..sccs.len()).map(|i| (i, 0)).collect();
    let mut rev: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for t in types {
        let from = scc_of[&t.id];
        let mut seen: HashSet<usize> = HashSet::new();
        for dep in refs_of(t) {
            let Some(&to) = scc_of.get(&dep) else {
                continue; // dangling, already reported
            };
            if to == from {
                continue;
            }
            if !seen.insert(to) {
                continue;
            }
            *indeg.entry(from).or_insert(0) += 1;
            rev.entry(to).or_default().push(from);
        }
    }

    // 4. Kahn's on the SCC-DAG. Tiebreak by alphabetical SCC representative
    //    so the topo order is deterministic and matches the pre-Tarjan
    //    behaviour for cycle-free graphs.
    let mut ready: Vec<usize> = indeg
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| *i)
        .collect();
    sort_ready_desc(&mut ready, &scc_rep);

    let mut scc_order: Vec<usize> = Vec::with_capacity(sccs.len());
    while let Some(i) = ready.pop() {
        scc_order.push(i);
        if let Some(deps) = rev.get(&i) {
            for d in deps {
                if let Some(n) = indeg.get_mut(d) {
                    *n -= 1;
                    if *n == 0 {
                        ready.push(*d);
                    }
                }
            }
            sort_ready_desc(&mut ready, &scc_rep);
        }
    }

    // 5. Emit members. Within each SCC sort alphabetically. Collect any
    //    SCC that's a recursion group (>1 member or single member with a
    //    self-edge) for surfacing as info-level diagnostics.
    let mut sorted: Vec<NamedType> = Vec::with_capacity(types.len());
    let mut recursion_groups: Vec<Vec<String>> = Vec::new();
    for &i in &scc_order {
        let scc = &sccs[i];
        let is_recursion = scc.len() > 1 || (scc.len() == 1 && has_self_loop(&by_id, &scc[0]));
        if is_recursion {
            let mut members = scc.clone();
            members.sort();
            recursion_groups.push(members);
        }
        let mut members: Vec<&NamedType> =
            scc.iter().filter_map(|id| by_id.get(id).copied()).collect();
        members.sort_by(|a, b| a.id.cmp(&b.id));
        for m in members {
            sorted.push((*m).clone());
        }
    }
    (sorted, recursion_groups)
}

fn sort_ready_desc(ready: &mut [usize], scc_rep: &[String]) {
    // Sort DESC so `pop()` returns the alphabetically-smallest representative.
    ready.sort_by(|a, b| scc_rep[*b].cmp(&scc_rep[*a]));
}

fn has_self_loop(by_id: &HashMap<String, &NamedType>, id: &str) -> bool {
    let Some(nt) = by_id.get(id) else {
        return false;
    };
    refs_of(nt).into_iter().any(|r| r == id)
}

/// Tarjan's strongly-connected-components algorithm. Visits nodes in
/// alphabetical order so the resulting SCC list is deterministic.
fn tarjan_sccs(
    types: &[NamedType],
    by_id: &HashMap<String, &NamedType>,
    known: &HashSet<String>,
) -> Vec<Vec<String>> {
    let mut state = TarjanState {
        index_counter: 0,
        indices: HashMap::new(),
        lowlinks: HashMap::new(),
        on_stack: HashSet::new(),
        stack: Vec::new(),
        sccs: Vec::new(),
    };
    let mut order: Vec<&String> = types.iter().map(|t| &t.id).collect();
    order.sort();
    for id in order {
        if !state.indices.contains_key(id) {
            strongconnect(id, by_id, known, &mut state);
        }
    }
    state.sccs
}

struct TarjanState {
    index_counter: usize,
    indices: HashMap<String, usize>,
    lowlinks: HashMap<String, usize>,
    on_stack: HashSet<String>,
    stack: Vec<String>,
    sccs: Vec<Vec<String>>,
}

fn strongconnect(
    id: &str,
    by_id: &HashMap<String, &NamedType>,
    known: &HashSet<String>,
    s: &mut TarjanState,
) {
    s.indices.insert(id.to_string(), s.index_counter);
    s.lowlinks.insert(id.to_string(), s.index_counter);
    s.index_counter += 1;
    s.stack.push(id.to_string());
    s.on_stack.insert(id.to_string());

    if let Some(t) = by_id.get(id) {
        // Visit neighbours in alphabetical order for stable lowlink choice.
        let mut deps: Vec<TypeRef> = refs_of(t)
            .into_iter()
            .filter(|d| known.contains(d))
            .collect();
        deps.sort();
        deps.dedup();
        for dep in deps {
            if !s.indices.contains_key(&dep) {
                strongconnect(&dep, by_id, known, s);
                let dep_low = s.lowlinks[&dep];
                let cur_low = s.lowlinks[id];
                s.lowlinks.insert(id.to_string(), cur_low.min(dep_low));
            } else if s.on_stack.contains(&dep) {
                let dep_idx = s.indices[&dep];
                let cur_low = s.lowlinks[id];
                s.lowlinks.insert(id.to_string(), cur_low.min(dep_idx));
            }
        }
    }

    if s.indices[id] == s.lowlinks[id] {
        let mut scc = Vec::new();
        loop {
            let w = s.stack.pop().expect("non-empty stack");
            s.on_stack.remove(&w);
            let is_root = w == id;
            scc.push(w);
            if is_root {
                break;
            }
        }
        s.sccs.push(scc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_ir::{
        AdditionalProperties, ArrayConstraints, ArrayType, ObjectConstraints, ObjectType, Property,
        TypeDef,
    };

    fn obj(id: &str, props: &[(&str, &str)]) -> NamedType {
        NamedType {
            id: id.into(),
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
                properties: props
                    .iter()
                    .map(|(n, t)| Property {
                        name: (*n).into(),
                        r#type: (*t).into(),
                        required: false,
                        title: None,
                        description: None,
                        deprecated: false,
                        read_only: false,
                        write_only: false,
                        external_docs: None,
                        default: None,
                        examples: vec![],
                        extensions: vec![],
                    })
                    .collect(),
                pattern_properties: vec![],
                additional_properties: AdditionalProperties::Forbidden,
                property_names: None,
                constraints: ObjectConstraints::default(),
            }),
            extensions: vec![],
            location: None,
        }
    }

    fn prim(id: &str) -> NamedType {
        NamedType {
            id: id.into(),
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
            definition: TypeDef::Primitive(forge_ir::PrimitiveType {
                kind: forge_ir::PrimitiveKind::String,
                constraints: forge_ir::PrimitiveConstraints::default(),
            }),
            extensions: vec![],
            location: None,
        }
    }

    fn arr(id: &str, items: &str) -> NamedType {
        NamedType {
            id: id.into(),
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
            definition: TypeDef::Array(ArrayType {
                items: items.into(),
                constraints: ArrayConstraints::default(),
            }),
            extensions: vec![],
            location: None,
        }
    }

    #[test]
    fn topo_simple() {
        // Pets -> Pet -> id (string)
        let v = vec![
            arr("Pets", "Pet"),
            obj("Pet", &[("id", "id_str")]),
            prim("id_str"),
        ];
        let (sorted, groups) = topo_sort_with_sccs(&v);
        let ids: Vec<&str> = sorted.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["id_str", "Pet", "Pets"]);
        assert!(groups.is_empty());
    }

    #[test]
    fn topo_alphabetical_tiebreak() {
        let v = vec![prim("b"), prim("a"), prim("c")];
        let (sorted, groups) = topo_sort_with_sccs(&v);
        assert_eq!(
            sorted.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn mutual_recursion_emits_as_group() {
        let v = vec![obj("A", &[("b", "B")]), obj("B", &[("a", "A")])];
        let (sorted, groups) = topo_sort_with_sccs(&v);
        let ids: Vec<&str> = sorted.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["A", "B"]);
        assert_eq!(groups, vec![vec!["A".to_string(), "B".to_string()]]);
    }

    #[test]
    fn self_recursion_flagged_as_group() {
        let v = vec![obj("Tree", &[("child", "Tree")])];
        let (sorted, groups) = topo_sort_with_sccs(&v);
        assert_eq!(
            sorted.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["Tree"]
        );
        assert_eq!(groups, vec![vec!["Tree".to_string()]]);
    }

    #[test]
    fn recursion_group_keeps_dependencies_first() {
        // Leaf -> nothing. Pair: A <-> B, both ref Leaf. Wrapper -> A.
        let v = vec![
            obj("A", &[("b", "B"), ("leaf", "Leaf")]),
            obj("B", &[("a", "A"), ("leaf", "Leaf")]),
            prim("Leaf"),
            obj("Wrapper", &[("inner", "A")]),
        ];
        let (sorted, groups) = topo_sort_with_sccs(&v);
        let ids: Vec<&str> = sorted.iter().map(|t| t.id.as_str()).collect();
        // Leaf has no deps; the {A, B} SCC depends on Leaf; Wrapper depends
        // on the {A, B} SCC.
        assert_eq!(ids, vec!["Leaf", "A", "B", "Wrapper"]);
        assert_eq!(groups, vec![vec!["A".to_string(), "B".to_string()]]);
    }
}
