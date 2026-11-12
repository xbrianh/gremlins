use serde::Serialize;

use crate::config::{ComprehensibilityDimensionDef, DragConfig};
use crate::scoring::DragNode;
use crate::util::truncate;

/// Serializable mirror of [`DragNode`] for JSON output.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum DragNodeOutput {
    Module {
        name: String,
        file: String,
        token_count: usize,
        footprint: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        comprehensibility: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ci_lower: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        ci_upper: Option<f64>,
        drag: f64,
    },
    Container {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        maint_score: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        maint_ci_lower: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        maint_ci_upper: Option<f64>,
        drag: f64,
        children: Vec<DragNodeOutput>,
    },
}

impl From<&DragNode> for DragNodeOutput {
    fn from(node: &DragNode) -> Self {
        match node {
            DragNode::Module {
                name,
                file,
                token_count,
                footprint,
                comprehensibility,
                ci_lower,
                ci_upper,
                drag,
            } => DragNodeOutput::Module {
                name: name.clone(),
                file: file.clone(),
                token_count: *token_count,
                footprint: *footprint,
                comprehensibility: *comprehensibility,
                ci_lower: *ci_lower,
                ci_upper: *ci_upper,
                drag: *drag,
            },
            DragNode::Container {
                name,
                maint_score,
                maint_ci_lower,
                maint_ci_upper,
                drag,
                children,
            } => DragNodeOutput::Container {
                name: name.clone(),
                maint_score: *maint_score,
                maint_ci_lower: *maint_ci_lower,
                maint_ci_upper: *maint_ci_upper,
                drag: *drag,
                children: children.iter().map(DragNodeOutput::from).collect(),
            },
        }
    }
}

/// Weighting constants reported alongside the tree in JSON output.
#[derive(Serialize)]
pub(crate) struct ReportConfig {
    pub w2: f64,
    pub m: f64,
    pub p: f64,
}

/// Print a self-contained explanation of the drag calculation: the formula,
/// the comprehensibility and maintainability surveys, and how each factor
/// responds to change (curves). This is the single source of truth for the
/// drag model; `gremlins-drag --explain` feeds the drag-optimizer pipeline so
/// its prompts never hardcode a second copy.
pub(crate) fn print_explain(config: &DragConfig) {
    let w2 = config.weights.w2;
    let m = config.weights.m;
    let p = config.weights.p;
    let factor_footprint = |f: usize| -> f64 { (1.0 + f as f64).powf(w2) };
    let factor_comp = |c: f64| -> f64 { (m * (1.0 - c)).exp() };
    let factor_maint = |c: f64| -> f64 { (p * (1.0 - c)).exp() };

    println!("# Drag model\n");
    println!("## Formula\n");
    println!("  drag(m) = (1+footprint)^w2 · exp(m·(1−comprehensibility))");
    println!("  drag(C) = exp(p·(1−maint)) · Σ_{{direct modules}} drag(m) + Σ_{{child containers}} drag(C')");
    println!("  (each module pays exactly one maintainability penalty — from its");
    println!("   immediate directory container; nesting is captured by summing");
    println!("   child container drags up the tree, not by multiplying ancestors)\n");
    println!("  The total minimized is the root drag.\n");
    println!("  `footprint` is the token count of a module plus every module it");
    println!("  transitively reaches (including itself). Higher drag means the");
    println!("  module or package is harder to maintain and change.");
    println!("  Token counts are computed with `#[cfg(test)]` items stripped: test code");
    println!("  never contributes to token_count or footprint, but it does affect the");
    println!("  test_hygiene comprehensibility dimension.\n");

    let comp_dims: &[ComprehensibilityDimensionDef] = &config.comprehensibility.dimensions;
    let n_dims = comp_dims.len();
    println!("## Comprehensibility survey\n");
    if !config.comprehensibility.preamble.is_empty() {
        println!("### Preamble\n");
        for line in config.comprehensibility.preamble.lines() {
            println!("  {line}");
        }
        println!();
    }
    if !config.comprehensibility.generic_anchors.is_empty() {
        println!("### Generic anchors (0–4 scale)\n");
        for line in config.comprehensibility.generic_anchors.lines() {
            println!("  {line}");
        }
        println!();
    }
    println!("  Comprehensibility is scored by an LLM survey on {n_dims} dimensions, each");
    println!(
        "  rated 0–4 and normalized over {} points ({n_dims}×4):\n",
        n_dims * 4
    );
    for (i, dim) in comp_dims.iter().enumerate() {
        println!("  {}. {}  — {}", i + 1, dim.label, dim.description);
        if let Some(anchors) = config.comprehensibility.structural_anchors.get(&dim.key) {
            println!("     Anchors:");
            for line in anchors.lines() {
                println!("     {line}");
            }
        }
    }
    println!();
    println!("  A FILE SHAPE skeleton is prepended to the scoring prompt: total lines,");
    println!("  function count, submodule count, test-function count, largest function");
    println!("  span, test ratio, concern clusters, and the item outline. Structural");
    println!("  pathologies — god-files, god-functions, inline test bloat, zero submodules —");
    println!("  depress the score directly.\n");

    let n_maint = config.maintainability.dimensions.len();
    println!("## Directory maintainability survey\n");
    if !config.maintainability.preamble.is_empty() {
        println!("### Preamble\n");
        for line in config.maintainability.preamble.lines() {
            println!("  {line}");
        }
        println!();
    }
    println!("  Directory maintainability is scored on {n_maint} dimensions, each 0–4,");
    println!(
        "  normalized over {} points ({n_maint}×4). Every directory containing",
        n_maint * 4
    );
    println!("  source files (including package roots and single-file directories)");
    println!("  carries its own score, computed only when skip_maintainability is unset.\n");
    for (i, dim) in config.maintainability.dimensions.iter().enumerate() {
        println!("  {}. {}", i + 1, dim.description);
        if !dim.anchors.is_empty() {
            println!("     Anchors:");
            for line in dim.anchors.lines() {
                println!("     {line}");
            }
        }
    }

    println!("# Drag curves (w2={w2}, m={m}, p={p})\n");
    println!("  Footprint factor ((1+footprint)^w2, w2={w2}):");
    for footprint in [0, 5, 10, 50, 100, 500, 1000, 5000] {
        println!(
            "    {footprint:>5} footprint  → {:>10.3}",
            factor_footprint(footprint)
        );
    }

    println!("\n  Comprehensibility factor (exp(m·(1−comp)), m={m}):");
    for comp in [0.10, 0.25, 0.50, 0.75, 0.90, 1.00] {
        println!("    {comp:.2}  → {:>10.3}", factor_comp(comp));
    }

    println!("\n  Maintainability factor (exp(p·(1−maint)), p={p}):");
    for maint in [0.10, 0.25, 0.50, 0.75, 0.90, 1.00] {
        println!("    {maint:.2}  → {:>10.3}", factor_maint(maint));
    }

    println!("\n  All factors are multiplied together; larger values mean more drag.");
    println!("  Comprehensibility below 1.0 amplifies drag by up to exp(m).");
    println!("  Maintainability below 1.0 amplifies directory drag by up to exp(p).");
}

pub(crate) fn print_human_table(tree: &DragNode, skip_maint: bool) {
    // Only show the "comp" column when EVERY module has a score.
    let has_comp = tree_has_comp(tree);
    let has_maint = !skip_maint && tree_has_maint(tree);

    match (has_comp, has_maint) {
        (true, true) => {
            println!(
                "{:<55} {:>8} {:>10} {:>6} {:>20} {:>14}",
                "module", "tokens", "footprint", "comp", "maint", "drag"
            );
            println!("{}", "-".repeat(118));
        }
        (true, false) => {
            println!(
                "{:<55} {:>8} {:>10} {:>6} {:>14}",
                "module", "tokens", "footprint", "comp", "drag"
            );
            println!("{}", "-".repeat(97));
        }
        (false, true) => {
            println!(
                "{:<55} {:>8} {:>10} {:>20} {:>14}",
                "module", "tokens", "footprint", "maint", "drag"
            );
            println!("{}", "-".repeat(111));
        }
        (false, false) => {
            println!(
                "{:<55} {:>8} {:>10} {:>14}",
                "module", "tokens", "footprint", "drag"
            );
            println!("{}", "-".repeat(90));
        }
    }

    print_node(tree, 0, has_comp, has_maint);
}

/// Check whether any container in the tree has a maintainability score.
fn tree_has_maint(node: &DragNode) -> bool {
    match node {
        DragNode::Module { .. } => false,
        DragNode::Container {
            maint_score,
            children,
            ..
        } => maint_score.is_some() || children.iter().any(tree_has_maint),
    }
}

/// Check whether every module in the tree has a comprehensibility score.
fn tree_has_comp(node: &DragNode) -> bool {
    match node {
        DragNode::Module {
            comprehensibility, ..
        } => comprehensibility.is_some(),
        DragNode::Container { children, .. } => children.iter().all(tree_has_comp),
    }
}

/// Recursively print a drag tree node with indentation.
fn print_node(node: &DragNode, depth: usize, has_comp: bool, has_maint: bool) {
    let indent = "  ".repeat(depth);

    match node {
        DragNode::Module {
            name,
            token_count,
            footprint,
            comprehensibility,
            drag,
            ..
        } => {
            // elide the common package prefix for display brevity
            let display = truncate(name, (54usize).saturating_sub(depth * 2));
            let w = (55usize).saturating_sub(depth * 2);
            match (has_comp, has_maint) {
                (true, true) => {
                    let c = comprehensibility.expect("has_comp guarantees Some");
                    println!(
                        "{indent}{:<w$} {:>8} {:>10} {:>6.2} {:>20} {:>14.1}",
                        display,
                        token_count,
                        footprint,
                        c,
                        "",
                        drag,
                        w = w,
                    );
                }
                (true, false) => {
                    let c = comprehensibility.expect("has_comp guarantees Some");
                    println!(
                        "{indent}{:<w$} {:>8} {:>10} {:>6.2} {:>14.1}",
                        display,
                        token_count,
                        footprint,
                        c,
                        drag,
                        w = w,
                    );
                }
                (false, true) => {
                    println!(
                        "{indent}{:<w$} {:>8} {:>10} {:>20} {:>14.1}",
                        display,
                        token_count,
                        footprint,
                        "",
                        drag,
                        w = w,
                    );
                }
                (false, false) => {
                    println!(
                        "{indent}{:<w$} {:>8} {:>10} {:>14.1}",
                        display,
                        token_count,
                        footprint,
                        drag,
                        w = w,
                    );
                }
            }
        }
        DragNode::Container {
            name,
            maint_score,
            maint_ci_lower,
            maint_ci_upper,
            drag,
            children,
        } => {
            // Print container header (skip nameless workspace root).
            if !name.is_empty() {
                let display = truncate(name, (54usize).saturating_sub(depth * 2));
                let w = (55usize).saturating_sub(depth * 2);
                let maint = maint_score.map(|s| {
                    if let (Some(lo), Some(hi)) = (maint_ci_lower, maint_ci_upper) {
                        format!(
                            "{s:.2} ({:.2}–{:.2})",
                            lo.clamp(0.0, 1.0),
                            hi.clamp(0.0, 1.0)
                        )
                    } else {
                        format!("{s:.2}")
                    }
                });
                match (has_comp, has_maint) {
                    (true, true) => {
                        println!(
                            "{indent}{:<w$} {:>8} {:>10} {:>6} {:>20} {:>14.1}",
                            display,
                            "",
                            "",
                            "",
                            maint.as_deref().unwrap_or(""),
                            drag,
                            w = w,
                        );
                    }
                    (true, false) => {
                        println!(
                            "{indent}{:<w$} {:>8} {:>10} {:>6} {:>14.1}",
                            display,
                            "",
                            "",
                            "",
                            drag,
                            w = w,
                        );
                    }
                    (false, true) => {
                        println!(
                            "{indent}{:<w$} {:>8} {:>10} {:>20} {:>14.1}",
                            display,
                            "",
                            "",
                            maint.as_deref().unwrap_or(""),
                            drag,
                            w = w,
                        );
                    }
                    (false, false) => {
                        println!(
                            "{indent}{:<w$} {:>8} {:>10} {:>14.1}",
                            display,
                            "",
                            "",
                            drag,
                            w = w,
                        );
                    }
                }
            }

            for child in children.iter() {
                print_node(
                    child,
                    if name.is_empty() { depth } else { depth + 1 },
                    has_comp,
                    has_maint,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_fields_round_trip_through_json() {
        // ModuleDrag is not directly serialized, but DragNodeOutput is.
        // Build a DragNode::Module with CI fields and round-trip through
        // DragNodeOutput → serde_json.
        let node = DragNode::Module {
            name: "test_mod".into(),
            file: "src/test.rs".into(),
            token_count: 42,
            footprint: 100,
            comprehensibility: Some(0.75),
            ci_lower: Some(0.70),
            ci_upper: Some(0.80),
            drag: 1.5,
        };
        let output = DragNodeOutput::from(&node);
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "module");
        assert_eq!(v["name"], "test_mod");
        assert!((v["comprehensibility"].as_f64().unwrap() - 0.75).abs() < 1e-12);
        assert!((v["ci_lower"].as_f64().unwrap() - 0.70).abs() < 1e-12);
        assert!((v["ci_upper"].as_f64().unwrap() - 0.80).abs() < 1e-12);

        // None CI fields are omitted entirely by skip_serializing_if.
        let node_no_ci = DragNode::Module {
            name: "no_ci".into(),
            file: "src/no_ci.rs".into(),
            token_count: 1,
            footprint: 1,
            comprehensibility: None,
            ci_lower: None,
            ci_upper: None,
            drag: 0.0,
        };
        let output_no_ci = DragNodeOutput::from(&node_no_ci);
        let json_no_ci = serde_json::to_string(&output_no_ci).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&json_no_ci).unwrap();
        let obj = v2.as_object().unwrap();
        assert!(!obj.contains_key("comprehensibility"));
        assert!(!obj.contains_key("ci_lower"));
        assert!(!obj.contains_key("ci_upper"));
    }

    #[test]
    fn container_ci_fields_round_trip_through_json() {
        // Container CI fields round-trip through DragNodeOutput → JSON.
        let node = DragNode::Container {
            name: "my_pkg".into(),
            maint_score: Some(0.72),
            maint_ci_lower: Some(0.61),
            maint_ci_upper: Some(0.83),
            drag: 42.0,
            children: vec![],
        };
        let output = DragNodeOutput::from(&node);
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "container");
        assert_eq!(v["name"], "my_pkg");
        assert!((v["maint_score"].as_f64().unwrap() - 0.72).abs() < 1e-12);
        assert!((v["maint_ci_lower"].as_f64().unwrap() - 0.61).abs() < 1e-12);
        assert!((v["maint_ci_upper"].as_f64().unwrap() - 0.83).abs() < 1e-12);

        // None CI fields are omitted.
        let node_no_ci = DragNode::Container {
            name: "orphan".into(),
            maint_score: None,
            maint_ci_lower: None,
            maint_ci_upper: None,
            drag: 1.0,
            children: vec![],
        };
        let output_no_ci = DragNodeOutput::from(&node_no_ci);
        let json_no_ci = serde_json::to_string(&output_no_ci).unwrap();
        let v2: serde_json::Value = serde_json::from_str(&json_no_ci).unwrap();
        let obj = v2.as_object().unwrap();
        assert!(!obj.contains_key("maint_score"));
        assert!(!obj.contains_key("maint_ci_lower"));
        assert!(!obj.contains_key("maint_ci_upper"));
    }
}
