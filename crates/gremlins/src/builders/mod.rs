//! Builder API for constructing [`GremlinDefinition`] and stage trees
//! ergonomically from Rust code.
//!
//! # Quick start
//!
//! ```ignore
//! use gremlins::builders::*;
//!
//! let def = DefinitionBuilder::new("demo", "xai:grok-4")
//!     .stage(
//!         AgentBuilder::new("plan")
//!             .prompt("write the plan to {plan}")
//!             .bind("plan", artifact("artifact://plan.md"))
//!             .build()
//!             .unwrap(),
//!     )
//!     .stage(
//!         ExecBuilder::new("run")
//!             .cmd("cat {plan}")
//!             .interpolate("plan", content("artifact://plan.md"))
//!             .build()
//!             .unwrap(),
//!     )
//!     .build()
//!     .unwrap();
//! ```

pub mod agent;
pub mod artifacts;
pub mod composite;
pub mod definition;
pub mod exec;
pub mod recipes;

// Re-export everything at the module root
pub use agent::AgentBuilder;
pub use artifacts::{artifact, content, BindTarget, InterpolationValue};
pub use composite::{LoopBuilder, ParallelBuilder, SequenceBuilder};
pub use definition::{BootstrapBuilder, DefinitionBuilder, LandBuilder};
pub use exec::ExecBuilder;
pub use recipes::{implement, plan, plan_gh, verify};
