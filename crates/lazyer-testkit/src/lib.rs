//! Test and benchmark support: synthetic repositories, the real-git reference
//! backend, end-state snapshots and the scenarios every backend must pass.

pub mod baseline;
pub mod differential;
pub mod fallback;
pub mod fixture;
pub mod git;
pub mod gitcli;
pub mod rng;
pub mod scenario;
pub mod snapshot;

pub use differential::{differential, Verdict};
pub use fixture::{Fixture, Profile};
pub use gitcli::GitCli;
pub use scenario::Scenario;
