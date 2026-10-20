#![deny(unreachable_pub)]
#![allow(clippy::double_must_use)]
pub mod artifacts;
pub mod builders;
pub mod clients;
pub mod config;
pub mod core;
pub mod definition;
pub mod executor;
pub mod landscape;
pub mod model_uri;
pub mod prelude;
pub mod schemas;
pub mod stage_spec;

/// Re-exports of the `rig-core` completion types used by downstream binaries
/// (e.g. `gremlins-drag`). Depending on these through `gremlins` avoids a
/// second, independently-versioned `rig-core` in those crates.
pub mod rig {
    pub use rig_core::completion::{CompletionRequest, CompletionResponse, Message};
}

#[cfg(test)]
mod test_support;
