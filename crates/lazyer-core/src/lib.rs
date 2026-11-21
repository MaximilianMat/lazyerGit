//! lazyerGit engine.
//!
//! `api` is the contract every backend implements: the reference backend
//! (real git, in `lazyer-testkit`) and the native engine in this crate. The
//! differential tests run the same scenario through both and require an
//! identical end state.

pub mod api;
pub mod native;

pub use api::*;
pub use native::Native;
