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

/// Build version shared by CLI and daemon.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod test_support;
