//! Backend implementations and the provider registry.
//!
//! Each backend lives in its own file and exports a `build` function matching
//! [`BuildFn`]. The [`backends!`] macro generates both the `pub mod`
//! declarations and a [`registry`] function that maps provider names to their
//! constructors — adding a backend is a single-line edit in the macro
//! invocation below.

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::clients::backend::Backend;

/// The signature every provider's `build` function must match.
pub type BuildFn = fn(
    model: &str,
    native_block: &HashMap<String, Vec<String>>,
    extra_params: &IndexMap<String, String>,
) -> Result<Arc<dyn Backend>, String>;

/// Declare backend modules and generate a static registry.
///
/// Each entry is `module: "provider_name" => path::to::Struct::build`.
/// The macro expands to `pub mod module;` for each entry and a single
/// `registry()` function returning `Vec<(&'static str, BuildFn)>`.
macro_rules! backends {
    ($($module:ident : $name:literal => $build:path),* $(,)?) => {
        $(pub mod $module;)*

        /// Return every registered backend, in declaration order.
        pub fn registry() -> Vec<(&'static str, BuildFn)> {
            vec![$(
                ($name, $build as BuildFn),
            )*]
        }
    };
}

backends! {
    cmd: "cmd" => cmd::CmdBackend::build,
    copilot: "copilot" => copilot::CopilotBackend::build,
    openai: "openai" => openai::OpenAiBackend::build,
    xai: "xai" => xai::XaiBackend::build,
    openrouter: "openrouter" => openrouter::OpenRouterBackend::build,
    azure: "azure" => azure::AzureBackend::build,
}
