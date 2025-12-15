//! Core CAS store operations.
//!
//! Mapping note (for reviewers):
//! - Old: `core/store/cas/store_impl.rs`
//! - New:
//!   - store/load + object IO: `store_impl/objects.rs`
//!   - owner refs: `store_impl/refs.rs`
//!   - lookup keys + listing: `store_impl/keys.rs`
//!   - index schema + health + rebuild: `store_impl/index.rs`
//!   - runtime manifests + env projections: `store_impl/manifest.rs`

mod index;
mod keys;
mod manifest;
mod objects;
mod refs;
