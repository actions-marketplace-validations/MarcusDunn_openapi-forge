//! `wasmtime::component::bindgen!`-generated bindings for both worlds.
//!
//! The two worlds share the `host-api` import and the `types` interface.
//! Bindgen still emits separate Rust modules per world, so we call the macro
//! twice. The conversions in [`super::convert`] cover both module hierarchies.

#![allow(clippy::all)]
#![allow(missing_docs)]
#![allow(missing_debug_implementations)]

pub mod transformer {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "ir-transformer",
        imports: { default: trappable },
    });
}

pub mod generator {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "code-generator",
        imports: { default: trappable },
    });
}

// Convenience aliases — the imported interfaces are byte-identical between
// the two worlds, so we point at the transformer world's modules. Host-api
// and stage are imports; types is a shared interface.
pub use transformer::forge::plugin::host_api;
pub use transformer::forge::plugin::stage;
pub use transformer::forge::plugin::types;
