use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

// ---------------------------------------------------------------------------
// Module-level drag formula
// ---------------------------------------------------------------------------

/// Compute the multiplicative drag score for a module.
///
/// `drag = (1+footprint)^w2 · exp(m·(1−comp))`.
///
/// `footprint` is the module's own token count plus the token count of every
/// module transitively reachable from it (see [`compute_footprint`]).
pub(crate) fn compute_drag(w2: f64, m: f64, footprint: usize, comprehensibility: f64) -> f64 {
    let footprint_factor = (1.0 + footprint as f64).powf(w2);
    let comp_factor = (m * (1.0 - comprehensibility)).exp();
    footprint_factor * comp_factor
}
/// Compute `footprint(m)` for every module: the sum of token counts of `m`
/// and all modules transitively reachable from `m`, including the module
/// itself.
///
/// Self-edges are ignored, cycles terminate naturally via the visited set,
/// and a token count of zero is used for any reachable module missing from
/// the token map.
pub(crate) fn compute_footprint(
    edges: &[(String, String)],
    token_counts: &HashMap<String, usize>,
) -> HashMap<String, usize> {
    let mut adjacency: HashMap<String, Vec<String>> = HashMap::new();
    for (src, dst) in edges {
        if src == dst {
            continue;
        }
        adjacency.entry(src.clone()).or_default().push(dst.clone());
    }

    let mut nodes: HashSet<String> = adjacency.keys().cloned().collect();
    nodes.extend(token_counts.keys().cloned());

    let mut footprints: HashMap<String, usize> = HashMap::new();
    for node in &nodes {
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = adjacency.get(node).cloned().unwrap_or_default();

        while let Some(next) = stack.pop() {
            if next == *node || !visited.insert(next.clone()) {
                continue;
            }
            if let Some(children) = adjacency.get(&next) {
                stack.extend(children.iter().cloned());
            }
        }

        let total = token_counts.get(node).copied().unwrap_or(0)
            + visited
                .iter()
                .map(|m| token_counts.get(m).copied().unwrap_or(0))
                .sum::<usize>();
        footprints.insert(node.clone(), total);
    }

    footprints
}

// ---------------------------------------------------------------------------
// Drag tree types
// ---------------------------------------------------------------------------

/// A node in the recursive drag tree.
///
/// Leaf [`Module`] nodes hold individual module drag scores (unchanged
/// formula).  [`Container`] nodes aggregate descendant drag with a
/// maintainability multiplier applied exactly once per module — from the
/// module's immediate container.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum DragNode {
    Module {
        name: String,
        file: String,
        token_count: usize,
        footprint: usize,
        comprehensibility: Option<f64>,
        drag: f64,
    },
    Container {
        name: String,
        /// Only package-level containers carry their own maint score;
        /// subtree containers return `None` and inherit from their
        /// nearest package ancestor.
        #[serde(skip_serializing_if = "Option::is_none")]
        maint_score: Option<f64>,
        drag: f64,
        children: Vec<DragNode>,
    },
}

// ---------------------------------------------------------------------------
// Tree construction & drag computation
// ---------------------------------------------------------------------------

/// Build a drag tree from scored modules.
///
/// `module_pkg` maps each module's path to its package key.  Modules not
/// associated with any package are placed under a synthetic root.
///
/// The tree is walked bottom-up so every container's `drag` field is the
/// value of the recursive formula.
pub(crate) fn build_drag_tree(
    modules: &[super::ModuleDrag],
    module_pkg: &HashMap<String, String>,
    pkg_maints: &HashMap<String, f64>,
    pkg_name_for_key: &HashMap<String, String>,
    p: f64,
) -> DragNode {
    // ── 1. Build an intermediate trie from module paths ──────────────
    let mut root_trie = TrieNode::default();

    for (idx, m) in modules.iter().enumerate() {
        let segments: Vec<&str> = m.module.split("::").collect();
        root_trie.insert(&segments, idx);
    }

    // ── 2. Group by package: each pkg_key → list of module indices ──
    let mut pkg_module_indices: HashMap<String, Vec<usize>> = HashMap::new();
    let mut orphan_indices: Vec<usize> = Vec::new();
    for (idx, m) in modules.iter().enumerate() {
        if let Some(pkg_key) = module_pkg.get(&m.module) {
            pkg_module_indices
                .entry(pkg_key.clone())
                .or_default()
                .push(idx);
        } else {
            orphan_indices.push(idx);
        }
    }

    // ── 3. Build one container per package ──────────────────────────
    let mut package_containers: Vec<DragNode> = Vec::new();

    for (pkg_key, indices) in &pkg_module_indices {
        let pkg_display_name = pkg_name_for_key
            .get(pkg_key)
            .cloned()
            .unwrap_or_else(|| pkg_key.clone());
        let maint = pkg_maints.get(pkg_key).copied();

        // Find the longest common prefix among this package's modules.
        let common_prefix = longest_common_module_prefix(
            &indices
                .iter()
                .map(|&i| modules[i].module.as_str())
                .collect::<Vec<_>>(),
        );

        // Build a subtree trie for just this package's modules,
        // anchored at `common_prefix`.
        let mut sub_trie = TrieNode::default();
        for &idx in indices {
            let full: Vec<&str> = modules[idx].module.split("::").collect();
            // Strip the common prefix so the trie only contains the
            // package-internal hierarchy.
            let relative: Vec<&str> = if common_prefix.is_empty() {
                full
            } else {
                let prefix_segments: Vec<&str> = common_prefix.split("::").collect();
                full[prefix_segments.len()..].to_vec()
            };
            sub_trie.insert(&relative, idx);
        }

        let container = trie_to_drag_node(&sub_trie, modules, pkg_display_name.clone(), maint, p);

        // If the common prefix has multiple segments beyond the package
        // name, wrap in intermediate containers.  But the package name
        // itself is already the container name — any deeper hierarchy
        // was handled by trie_to_drag_node.
        package_containers.push(container);
    }

    // ── 4. Handle orphan modules (no package) ───────────────────────
    let orphan_nodes: Vec<DragNode> = orphan_indices
        .iter()
        .map(|&idx| {
            let m = &modules[idx];
            DragNode::Module {
                name: m.module.clone(),
                file: m.file.clone(),
                token_count: m.token_count,
                footprint: m.footprint,
                comprehensibility: m.comprehensibility,
                drag: m.drag,
            }
        })
        .collect();

    // ── 5. Assemble the root ────────────────────────────────────────
    let mut all_children: Vec<DragNode> = Vec::new();
    all_children.extend(package_containers);
    all_children.extend(orphan_nodes);

    if all_children.len() == 1 {
        return all_children.remove(0);
    }

    let total_drag: f64 = all_children.iter().map(drag_of).sum();
    DragNode::Container {
        name: String::new(),
        maint_score: None,
        drag: total_drag,
        children: all_children,
    }
}

/// Return the drag value of any node (module or container).
fn drag_of(node: &DragNode) -> f64 {
    match node {
        DragNode::Module { drag, .. } => *drag,
        DragNode::Container { drag, .. } => *drag,
    }
}

// ---------------------------------------------------------------------------
// Internal trie helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct TrieNode {
    /// Indices of modules whose path ends exactly at this trie node.
    module_indices: Vec<usize>,
    /// Child segments.
    children: BTreeMap<String, TrieNode>,
}

impl TrieNode {
    fn insert(&mut self, segments: &[&str], module_idx: usize) {
        if segments.is_empty() {
            self.module_indices.push(module_idx);
            return;
        }
        let child = self.children.entry(segments[0].to_string()).or_default();
        child.insert(&segments[1..], module_idx);
    }
}

/// Find the longest prefix shared by all module paths.
fn longest_common_module_prefix(paths: &[&str]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let split: Vec<Vec<&str>> = paths.iter().map(|p| p.split("::").collect()).collect();
    let min_len = split.iter().map(|v| v.len()).min().unwrap_or(0);
    let mut common = Vec::new();
    for i in 0..min_len {
        let seg = split[0][i];
        if split.iter().all(|v| v[i] == seg) {
            common.push(seg);
        } else {
            break;
        }
    }
    common.join("::")
}

/// Recursively convert a trie into a [`DragNode`] tree, computing container
/// drag bottom-up.
///
/// `maint_score` is the maintainability score this node should apply to its
/// direct module children.  Leaf modules' `drag` fields must already be
/// populated before calling this function.
fn trie_to_drag_node(
    trie: &TrieNode,
    modules: &[super::ModuleDrag],
    name: String,
    maint_score: Option<f64>,
    p: f64,
) -> DragNode {
    // Direct module children (leaf modules at this trie node).
    let direct_modules: Vec<DragNode> = trie
        .module_indices
        .iter()
        .map(|&idx| {
            let m = &modules[idx];
            DragNode::Module {
                name: m.module.clone(),
                file: m.file.clone(),
                token_count: m.token_count,
                footprint: m.footprint,
                comprehensibility: m.comprehensibility,
                drag: m.drag,
            }
        })
        .collect();

    // Child containers (subtries that have children of their own).
    let child_containers: Vec<DragNode> = trie
        .children
        .iter()
        .map(|(seg, child_trie)| {
            let child_name = if name.is_empty() {
                seg.clone()
            } else {
                format!("{name}::{seg}")
            };
            // Subtrees inherit this node's maint score.
            trie_to_drag_node(child_trie, modules, child_name, maint_score, p)
        })
        .collect();

    // If there are no children at all and at most one direct module,
    // we can flatten: return the module directly (or an empty container).
    if trie.children.is_empty() && direct_modules.len() <= 1 {
        if let Some(m) = direct_modules.into_iter().next() {
            return m;
        }
        // Empty: return a trivial container.
        return DragNode::Container {
            name,
            maint_score,
            drag: 0.0,
            children: Vec::new(),
        };
    }

    // Compute container drag bottom-up.
    // Partition children: flattened modules from sub-tries pay maint;
    // child containers already have maint baked in.
    let mut all_children: Vec<DragNode> = Vec::new();
    all_children.extend(direct_modules);
    all_children.extend(child_containers);

    let direct_module_drag_sum: f64 = all_children
        .iter()
        .filter_map(|c| match c {
            DragNode::Module { drag, .. } => Some(*drag),
            _ => None,
        })
        .sum();
    let child_container_drag_sum: f64 = all_children
        .iter()
        .filter_map(|c| match c {
            DragNode::Container { drag, .. } => Some(*drag),
            _ => None,
        })
        .sum();

    let maint_mult = maint_score.map_or(1.0, |s| (p * (1.0 - s)).exp());
    let drag = maint_mult * direct_module_drag_sum + child_container_drag_sum;

    // Sort children so modules come before containers for stable output.
    all_children.sort_by(|a, b| match (a, b) {
        (DragNode::Module { name: na, .. }, DragNode::Module { name: nb, .. }) => na.cmp(nb),
        (DragNode::Module { .. }, DragNode::Container { .. }) => std::cmp::Ordering::Less,
        (DragNode::Container { .. }, DragNode::Module { .. }) => std::cmp::Ordering::Greater,
        (DragNode::Container { name: na, .. }, DragNode::Container { name: nb, .. }) => na.cmp(nb),
    });

    DragNode::Container {
        name,
        maint_score,
        drag,
        children: all_children,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_drag / compute_footprint (unchanged) ────────────────

    #[test]
    fn compute_drag_uses_multiplicative_formula() {
        // (1+3)^0.5 * exp(1.0*(1-0.5)) = 2 * exp(0.5) ≈ 3.2974
        let drag = compute_drag(0.5, 1.0, 3, 0.5);
        let expected = 2.0 * 0.5_f64.exp();
        assert!((drag - expected).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_zero_footprint_with_comp() {
        let drag = compute_drag(0.5, 1.0, 0, 0.5);
        // (1+0)^0.5 * exp(1.0*(1-0.5)) = 1 * exp(0.5) ≈ 1.6487
        assert!((drag - 0.5_f64.exp()).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_comp_factor_is_multiplicative() {
        // Sanity check that the comprehensibility factor is a multiplier,
        // not an additive term in log space.
        // (1+10)^0 * exp(2.0*(1-1.0)) = exp(0) = 1.0
        let no_comp = compute_drag(0.0, 2.0, 10, 1.0);
        // (1+10)^0 * exp(2.0*(1-0.5)) = exp(1.0) ≈ 2.718
        let with_comp = compute_drag(0.0, 2.0, 10, 0.5);
        assert!((no_comp - 1.0).abs() < 1e-12);
        assert!((with_comp - 1.0_f64.exp()).abs() < 1e-12);
    }

    #[test]
    fn footprint_transitive_diamond() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("a".to_string(), "c".to_string()),
            ("b".to_string(), "d".to_string()),
            ("c".to_string(), "d".to_string()),
        ];
        let tokens = HashMap::from([
            ("a".to_string(), 10usize),
            ("b".to_string(), 20usize),
            ("c".to_string(), 30usize),
            ("d".to_string(), 40usize),
        ]);
        let footprint = compute_footprint(&edges, &tokens);
        assert_eq!(footprint.get("a"), Some(&100));
        assert_eq!(footprint.get("b"), Some(&60));
        assert_eq!(footprint.get("c"), Some(&70));
        assert_eq!(footprint.get("d"), Some(&40));
    }

    #[test]
    fn footprint_cycle_stops_at_visited_set() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("c".to_string(), "a".to_string()),
        ];
        let tokens = HashMap::from([
            ("a".to_string(), 10usize),
            ("b".to_string(), 20usize),
            ("c".to_string(), 30usize),
        ]);
        let footprint = compute_footprint(&edges, &tokens);
        assert_eq!(footprint.get("a"), Some(&60));
        assert_eq!(footprint.get("b"), Some(&60));
        assert_eq!(footprint.get("c"), Some(&60));
    }

    #[test]
    fn footprint_self_edges_ignored() {
        let edges = vec![
            ("a".to_string(), "a".to_string()),
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "b".to_string()),
        ];
        let tokens = HashMap::from([("a".to_string(), 10usize), ("b".to_string(), 20usize)]);
        let footprint = compute_footprint(&edges, &tokens);
        assert_eq!(footprint.get("a"), Some(&30));
        assert_eq!(footprint.get("b"), Some(&20));
    }

    #[test]
    fn footprint_counts_out_of_target_modules() {
        // `b` is not part of the scored target set, but its token count is
        // still part of the transitive footprint for `a`.
        let edges = vec![("a".to_string(), "b".to_string())];
        let tokens = HashMap::from([("a".to_string(), 10usize), ("b".to_string(), 25usize)]);
        let footprint = compute_footprint(&edges, &tokens);
        assert_eq!(footprint.get("a"), Some(&35));
        assert_eq!(footprint.get("b"), Some(&25));
    }

    // ── Drag tree tests ─────────────────────────────────────────────

    /// Helper to make a ModuleDrag with the given fields.
    fn mdl(name: &str, drag: f64) -> super::super::ModuleDrag {
        super::super::ModuleDrag {
            module: name.to_string(),
            file: format!("/src/{}.rs", name.replace("::", "/")),
            token_count: 100,
            footprint: 0,
            comprehensibility: Some(0.5),
            drag,
        }
    }

    #[test]
    fn tree_with_maint_1_is_flat_sum() {
        // With maint=1.0, the tree drag equals the sum of module drags
        // regardless of nesting.
        let modules = vec![
            mdl("gremlins::config", 10.0),
            mdl("gremlins::clients::client", 5.0),
            mdl("gremlins::clients::provider", 7.0),
            mdl("gremlins::executor::gremlin", 20.0),
        ];
        let module_pkg: HashMap<String, String> = modules
            .iter()
            .map(|m| (m.module.clone(), "gremlins".to_string()))
            .collect();
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("gremlins".to_string(), 1.0);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("gremlins".to_string(), "gremlins".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5);

        // Root should be the "gremlins" container (only one package).
        let root_drag = drag_of(&tree);
        let flat_sum: f64 = modules.iter().map(|m| m.drag).sum();
        assert!(
            (root_drag - flat_sum).abs() < 1e-9,
            "root_drag={root_drag}, flat_sum={flat_sum}"
        );
    }

    #[test]
    fn tree_applies_maint_to_direct_modules_only() {
        // gremlins::config (direct child of gremlins) pays maint penalty.
        // gremlins::clients::* (in sub-container) also pay maint penalty
        // from gremlins (inherited).
        let modules = vec![
            mdl("gremlins::config", 10.0),
            mdl("gremlins::clients::client", 5.0),
            mdl("gremlins::clients::provider", 7.0),
        ];
        let module_pkg: HashMap<String, String> = modules
            .iter()
            .map(|m| (m.module.clone(), "gremlins".to_string()))
            .collect();
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("gremlins".to_string(), 0.8);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("gremlins".to_string(), "gremlins".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5);

        // maint_mult = exp(2.5 * 0.2) = exp(0.5) ≈ 1.6487212707
        let mult = (2.5 * 0.2_f64).exp();
        // clients container drag = mult * (5 + 7) = mult * 12
        let clients_drag = mult * 12.0;
        // root drag = mult * 10.0 + clients_drag
        let expected = mult * 10.0 + clients_drag;
        let root_drag = drag_of(&tree);
        assert!(
            (root_drag - expected).abs() < 1e-9,
            "root={root_drag}, expected={expected}"
        );

        // Verify the clients sub-container exists and has correct drag.
        if let DragNode::Container { children, .. } = &tree {
            let clients = children.iter().find(
                |c| matches!(c, DragNode::Container { name, .. } if name == "gremlins::clients"),
            );
            assert!(clients.is_some(), "expected gremlins::clients container");
            if let Some(DragNode::Container { drag, .. }) = clients {
                assert!((drag - clients_drag).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn tree_nested_subtrees_inherit_maint() {
        // Deep nesting: a::b::c::d — the innermost container inherits
        // maint from the package-level container.
        let modules = vec![
            mdl("pkg::a", 1.0),
            mdl("pkg::a::b", 2.0),
            mdl("pkg::a::b::c", 3.0),
            mdl("pkg::a::b::c::d", 4.0),
        ];
        let module_pkg: HashMap<String, String> = modules
            .iter()
            .map(|m| (m.module.clone(), "pkg".to_string()))
            .collect();
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("pkg".to_string(), 0.5);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("pkg".to_string(), "pkg".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5);

        // maint_mult = exp(2.5 * 0.5) = exp(1.25) ≈ 3.49034
        let mult = (2.5 * 0.5_f64).exp();
        // The tree should be:
        // pkg (maint=0.5)
        //   pkg::a (module, drag=1.0)
        //   pkg::a::b (container)
        //     pkg::a::b (module, drag=2.0)
        //     pkg::a::b::c (container)
        //       pkg::a::b::c (module, drag=3.0)
        //       pkg::a::b::c::d (module, drag=4.0)
        //
        // Bottom-up:
        // pkg::a::b::c drag = mult * (3 + 4) = mult * 7
        // pkg::a::b drag = mult * 2 + (pkg::a::b::c drag) = mult * 2 + mult * 7 = mult * 9
        // pkg drag = mult * 1 + (pkg::a::b drag) = mult * 1 + mult * 9 = mult * 10
        let expected = mult * 10.0;
        let root_drag = drag_of(&tree);
        assert!(
            (root_drag - expected).abs() < 1e-9,
            "root={root_drag}, expected={expected}"
        );
    }

    #[test]
    fn tree_multi_package_workspace_root() {
        let modules = vec![
            mdl("pkg_a::mod1", 10.0),
            mdl("pkg_a::mod2", 20.0),
            mdl("pkg_b::mod1", 30.0),
        ];
        let module_pkg: HashMap<String, String> = HashMap::from([
            ("pkg_a::mod1".to_string(), "pkg_a".to_string()),
            ("pkg_a::mod2".to_string(), "pkg_a".to_string()),
            ("pkg_b::mod1".to_string(), "pkg_b".to_string()),
        ]);
        let pkg_maints: HashMap<String, f64> =
            HashMap::from([("pkg_a".to_string(), 1.0), ("pkg_b".to_string(), 1.0)]);
        let pkg_names: HashMap<String, String> = HashMap::from([
            ("pkg_a".to_string(), "pkg_a".to_string()),
            ("pkg_b".to_string(), "pkg_b".to_string()),
        ]);

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5);

        // Should be a workspace root container with two children.
        assert!(matches!(tree, DragNode::Container { .. }));
        let root_drag = drag_of(&tree);
        assert!((root_drag - 60.0).abs() < 1e-9);
    }

    #[test]
    fn tree_single_module_returns_it_directly() {
        let modules = vec![mdl("lone_mod", 42.0)];
        let module_pkg = HashMap::new(); // no package
        let pkg_maints = HashMap::new();
        let pkg_names = HashMap::new();

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5);
        assert!(matches!(tree, DragNode::Module { .. }));
        assert!((drag_of(&tree) - 42.0).abs() < 1e-9);
    }

    #[test]
    fn longest_common_prefix_basic() {
        let paths = &["a::b::c", "a::b::d", "a::b::e"];
        assert_eq!(longest_common_module_prefix(paths), "a::b");
    }

    #[test]
    fn longest_common_prefix_single() {
        assert_eq!(longest_common_module_prefix(&["foo::bar"]), "foo::bar");
    }

    #[test]
    fn longest_common_prefix_none() {
        assert_eq!(longest_common_module_prefix(&["a::b", "c::d"]), "");
    }

    #[test]
    fn longest_common_prefix_empty() {
        assert_eq!(longest_common_module_prefix(&[]), "");
    }
}
