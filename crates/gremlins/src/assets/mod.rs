use phf::{phf_map, Map};

/// Compile-time map: prompt path → raw markdown/text content.
pub static PROMPTS: Map<&'static str, &'static str> = phf_map! {
    "address.md"                                => include_str!("data/prompts/address.md"),
    "analyze.md"                                => include_str!("data/prompts/analyze.md"),
    "assistant/setup.md"                        => include_str!("data/prompts/assistant/setup.md"),
    "bail_section.md"                           => include_str!("data/prompts/bail_section.md"),
    "bail_section_fix.md"                       => include_str!("data/prompts/bail_section_fix.md"),
    "ci_fix.md"                                 => include_str!("data/prompts/ci_fix.md"),
    "implement_local.md"                        => include_str!("data/prompts/implement_local.md"),
    "plan.md"                                   => include_str!("data/prompts/plan.md"),
    "verify_fix.md"                             => include_str!("data/prompts/verify_fix.md"),
};

/// Compile-time map: recipe name → raw YAML content.
pub(crate) static RECIPES: Map<&'static str, &'static str> = phf_map! {
    "implement"                     => include_str!("data/stages/implement.yaml"),
    "plan"                          => include_str!("data/stages/plan.yaml"),
    "verify"                        => include_str!("data/stages/verify.yaml")
};
