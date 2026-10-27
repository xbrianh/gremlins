use std::collections::{BTreeMap, HashMap, HashSet};

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
///
/// Serialization is handled by [`DragNodeOutput`] in `main.rs`, which
/// keeps the serde attributes out of this pure-computation module.
#[derive(Clone, Debug)]
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
        /// subtree containers are always `None`.
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
/// `module_pkg` maps each (module_path, file) pair to its package key.
/// Modules not associated with any package are placed under a synthetic
/// root.
///
/// The tree is walked bottom-up so every container's `drag` field is the
/// value of the recursive formula.
pub(crate) fn build_drag_tree(
    modules: &[super::ModuleDrag],
    module_pkg: &HashMap<(String, String), String>,
    pkg_maints: &HashMap<String, f64>,
    pkg_name_for_key: &HashMap<String, String>,
    p: f64,
    skip_maintainability: bool,
) -> DragNode {
    // ── 1. Group by package: each pkg_key → list of module indices ──
    let mut pkg_module_indices: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut orphan_indices: Vec<usize> = Vec::new();
    for (idx, m) in modules.iter().enumerate() {
        let key = (m.module.clone(), m.file.clone());
        if let Some(pkg_key) = module_pkg.get(&key) {
            pkg_module_indices
                .entry(pkg_key.clone())
                .or_default()
                .push(idx);
        } else {
            orphan_indices.push(idx);
        }
    }

    // ── 2. Build one container per package ──────────────────────────
    let mut package_containers: Vec<DragNode> = Vec::new();

    for (pkg_key, indices) in &pkg_module_indices {
        let pkg_display_name = pkg_name_for_key
            .get(pkg_key)
            .cloned()
            .unwrap_or_else(|| pkg_key.clone());
        // Determine separator from the first module's path.
        let module_paths: Vec<&str> = indices
            .iter()
            .map(|&i| modules[i].module.as_str())
            .collect();
        let sep = path_separator(&module_paths);

        let (maint, maint_mult) = if skip_maintainability {
            (None, 1.0)
        } else {
            let s = pkg_maints.get(pkg_key).copied().unwrap_or(0.5);
            (Some(s), (p * (1.0 - s)).exp())
        };

        // Find the longest common prefix among this package's modules.
        let common_prefix = longest_common_module_prefix(&module_paths);

        // Build a subtree trie for just this package's modules,
        // anchored at `common_prefix`.
        let mut sub_trie = TrieNode::default();
        for &idx in indices {
            let full = module_segments(&modules[idx].module);
            // Strip the common prefix so the trie only contains the
            // package-internal hierarchy. The container name is set
            // to the common prefix so child names are correct.
            let relative: Vec<&str> = if common_prefix.is_empty() {
                full
            } else {
                let prefix_segs = module_segments(&common_prefix);
                full[prefix_segs.len()..].to_vec()
            };
            sub_trie.insert(&relative, idx);
        }

        // Container name is the common prefix (derived from module
        // paths).  When the common prefix is empty fall back to the
        // package display name.
        let container_name = if common_prefix.is_empty() {
            pkg_display_name.clone()
        } else {
            common_prefix.clone()
        };

        let mut container =
            trie_to_drag_node(&sub_trie, modules, container_name, maint_mult, true, sep);

        // If the common prefix has more segments than the package
        // display name, wrap in intermediate containers.
        if !common_prefix.is_empty() {
            let prefix_segments = module_segments(&common_prefix);
            // Wrap from the deepest level up to the single-segment
            // package name, skipping the innermost container (already
            // created by trie_to_drag_node).
            let mut wrapped = container;
            for i in (0..prefix_segments.len()).rev() {
                let parent_name = prefix_segments[..=i].join(sep);
                if parent_name == common_prefix {
                    continue;
                }
                let drag = drag_of(&wrapped);
                wrapped = DragNode::Container {
                    name: parent_name,
                    maint_score: None,
                    drag,
                    children: vec![wrapped],
                };
            }
            // Set maint_score on the outermost wrapper (package-level container).
            if let DragNode::Container {
                ref mut maint_score,
                ..
            } = wrapped
            {
                *maint_score = maint;
            }
            package_containers.push(wrapped);
            continue;
        }

        // No wrapping: set maint_score on the trie container.
        if let DragNode::Container {
            ref mut maint_score,
            ..
        } = container
        {
            *maint_score = maint;
        }
        package_containers.push(container);
    }

    // ── 3. Handle orphan modules (no package) ───────────────────────
    // Build orphan modules through the same trie logic so ::-delimited
    // paths produce nested containers.
    let orphan_nodes = if orphan_indices.is_empty() {
        Vec::new()
    } else {
        let mut orphan_trie = TrieNode::default();
        for &idx in &orphan_indices {
            let segments = module_segments(&modules[idx].module);
            orphan_trie.insert(&segments, idx);
        }
        let orphan_sep = if orphan_indices.is_empty() {
            "::"
        } else {
            path_separator(&[modules[orphan_indices[0]].module.as_str()])
        };
        let orphan_root =
            trie_to_drag_node(&orphan_trie, modules, String::new(), 1.0, false, orphan_sep);
        match orphan_root {
            DragNode::Container { children, .. } => children,
            single => vec![single],
        }
    };

    // ── 4. Assemble the root ────────────────────────────────────────
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
    let sep = path_separator(paths);
    let split: Vec<Vec<&str>> = paths.iter().map(|p| module_segments(p)).collect();
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
    common.join(sep)
}

/// Split a module path into segments, using `::` if present, otherwise `.`.
fn module_segments(path: &str) -> Vec<&str> {
    if path.contains("::") {
        path.split("::").collect()
    } else {
        path.split('.').collect()
    }
}

/// Detect the path separator used in a set of module paths.
fn path_separator(paths: &[&str]) -> &'static str {
    if paths.iter().any(|p| p.contains("::")) {
        "::"
    } else {
        "."
    }
}

/// Recursively convert a trie into a [`DragNode`] tree, computing container
/// drag bottom-up.
///
/// `maint_mult` is the pre-computed maintainability multiplier applied to
/// this node's direct module children.  Subtree containers inherit the same
/// multiplier for drag computation but always have `maint_score: None`.
///
/// `is_package_root` prevents flattening so a one-module package still
/// gets a Container wrapper and pays its maintainability penalty.
fn trie_to_drag_node(
    trie: &TrieNode,
    modules: &[super::ModuleDrag],
    name: String,
    maint_mult: f64,
    is_package_root: bool,
    sep: &str,
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
                format!("{name}{sep}{seg}")
            };
            // Subtrees inherit the same maint_mult for drag computation
            // but always serialize `maint_score: None`.
            trie_to_drag_node(child_trie, modules, child_name, maint_mult, false, sep)
        })
        .collect();

    // Flatten non-package-root containers with at most one direct
    // module and no child subtrees.  Package roots always stay as
    // containers so the maintainability penalty is applied.
    if !is_package_root && trie.children.is_empty() && direct_modules.len() <= 1 {
        if let Some(m) = direct_modules.into_iter().next() {
            return m;
        }
        // Empty: return a trivial container.
        return DragNode::Container {
            name,
            maint_score: None,
            drag: 0.0,
            children: Vec::new(),
        };
    }

    // Compute container drag bottom-up.
    // Direct modules pay the maint multiplier; child containers already
    // have maint baked in from their own trie_to_drag_node call.
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

    let drag = maint_mult * direct_module_drag_sum + child_container_drag_sum;

    // Sort children: modules first, then containers; within each group
    // sort by drag descending so the highest-drag items appear first.
    all_children.sort_by(|a, b| {
        let a_is_module = matches!(a, DragNode::Module { .. });
        let b_is_module = matches!(b, DragNode::Module { .. });
        match (a_is_module, b_is_module) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                // Both same variant: sort by drag descending.
                let a_drag = drag_of(a);
                let b_drag = drag_of(b);
                b_drag
                    .partial_cmp(&a_drag)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        // Tie-break by name for determinism.
                        match (a, b) {
                            (
                                DragNode::Module { name: na, .. },
                                DragNode::Module { name: nb, .. },
                            ) => na.cmp(nb),
                            (
                                DragNode::Container { name: na, .. },
                                DragNode::Container { name: nb, .. },
                            ) => na.cmp(nb),
                            _ => std::cmp::Ordering::Equal,
                        }
                    })
            }
        }
    });

    DragNode::Container {
        name,
        maint_score: None,
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

    /// Helper: build a (module, file) → pkg_key map from a list.
    fn module_pkg_map(entries: &[(&str, &str)]) -> HashMap<(String, String), String> {
        entries
            .iter()
            .map(|(module, pkg)| {
                let file = format!("/src/{}.rs", module.replace("::", "/"));
                ((module.to_string(), file), pkg.to_string())
            })
            .collect()
    }

    /// Helper to make a Python ModuleDrag (dotted module path).
    fn mdl_py(name: &str, drag: f64) -> super::super::ModuleDrag {
        super::super::ModuleDrag {
            module: name.to_string(),
            file: format!("/{}.py", name.replace('.', "/")),
            token_count: 100,
            footprint: 0,
            comprehensibility: Some(0.5),
            drag,
        }
    }

    /// Helper: build a Python (module, file) → pkg_key map.
    fn module_pkg_map_py(entries: &[(&str, &str)]) -> HashMap<(String, String), String> {
        entries
            .iter()
            .map(|(module, pkg)| {
                let file = format!("/{}.py", module.replace('.', "/"));
                ((module.to_string(), file), pkg.to_string())
            })
            .collect()
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
        let module_pkg = module_pkg_map(&[
            ("gremlins::config", "gremlins"),
            ("gremlins::clients::client", "gremlins"),
            ("gremlins::clients::provider", "gremlins"),
            ("gremlins::executor::gremlin", "gremlins"),
        ]);
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("gremlins".to_string(), 1.0);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("gremlins".to_string(), "gremlins".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

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
        let module_pkg = module_pkg_map(&[
            ("gremlins::config", "gremlins"),
            ("gremlins::clients::client", "gremlins"),
            ("gremlins::clients::provider", "gremlins"),
        ]);
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("gremlins".to_string(), 0.8);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("gremlins".to_string(), "gremlins".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

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
        let module_pkg = module_pkg_map(&[
            ("pkg::a", "pkg"),
            ("pkg::a::b", "pkg"),
            ("pkg::a::b::c", "pkg"),
            ("pkg::a::b::c::d", "pkg"),
        ]);
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("pkg".to_string(), 0.5);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("pkg".to_string(), "pkg".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

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
        let module_pkg = module_pkg_map(&[
            ("pkg_a::mod1", "pkg_a"),
            ("pkg_a::mod2", "pkg_a"),
            ("pkg_b::mod1", "pkg_b"),
        ]);
        let pkg_maints: HashMap<String, f64> =
            HashMap::from([("pkg_a".to_string(), 1.0), ("pkg_b".to_string(), 1.0)]);
        let pkg_names: HashMap<String, String> = HashMap::from([
            ("pkg_a".to_string(), "pkg_a".to_string()),
            ("pkg_b".to_string(), "pkg_b".to_string()),
        ]);

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

        // Should be a workspace root container with two children.
        assert!(matches!(tree, DragNode::Container { .. }));
        let root_drag = drag_of(&tree);
        assert!((root_drag - 60.0).abs() < 1e-9);
    }

    #[test]
    fn tree_single_module_orphan_returns_it_directly() {
        let modules = vec![mdl("lone_mod", 42.0)];
        let module_pkg = HashMap::new(); // no package
        let pkg_maints = HashMap::new();
        let pkg_names = HashMap::new();

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);
        assert!(matches!(tree, DragNode::Module { .. }));
        assert!((drag_of(&tree) - 42.0).abs() < 1e-9);
    }

    #[test]
    fn tree_orphan_modules_nested_by_path() {
        // Orphans with :: should produce nested containers.
        let modules = vec![mdl("orphan::a", 10.0), mdl("orphan::b::c", 20.0)];
        let tree = build_drag_tree(
            &modules,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            2.5,
            false,
        );

        // Should be an "orphan" container with one module and one sub-container.
        assert!(matches!(tree, DragNode::Container { .. }));
        if let DragNode::Container { name, children, .. } = &tree {
            assert_eq!(name, "orphan");
            assert_eq!(children.len(), 2);
            let has_module_a = children
                .iter()
                .any(|c| matches!(c, DragNode::Module { name, .. } if name == "orphan::a"));
            assert!(has_module_a);
            let has_container_b = children
                .iter()
                .any(|c| matches!(c, DragNode::Container { name, .. } if name == "orphan::b"));
            assert!(has_container_b);
        }
    }

    #[test]
    fn tree_single_module_package_applies_maint() {
        // A one-module package must still get its maint penalty applied.
        let modules = vec![mdl("tiny_pkg::only_mod", 10.0)];
        let module_pkg = module_pkg_map(&[("tiny_pkg::only_mod", "tiny_pkg")]);
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("tiny_pkg".to_string(), 0.8);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("tiny_pkg".to_string(), "tiny_pkg".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

        // Should be a container (not flattened to module) so maint applies.
        assert!(matches!(tree, DragNode::Container { .. }));
        let mult = (2.5 * 0.2_f64).exp(); // exp(p * (1 - maint)) = exp(2.5 * 0.2)
        let expected = mult * 10.0;
        assert!(
            (drag_of(&tree) - expected).abs() < 1e-9,
            "expected={expected}, got={}",
            drag_of(&tree)
        );
    }

    #[test]
    fn tree_package_with_long_common_prefix_preserves_names() {
        // When modules share a prefix longer than the package name,
        // container names must include the full path.
        let modules = vec![
            mdl("pkg::deep::nested::a", 1.0),
            mdl("pkg::deep::nested::b::c", 2.0),
        ];
        let module_pkg = module_pkg_map(&[
            ("pkg::deep::nested::a", "pkg"),
            ("pkg::deep::nested::b::c", "pkg"),
        ]);
        let mut pkg_maints = HashMap::new();
        pkg_maints.insert("pkg".to_string(), 1.0);
        let mut pkg_names = HashMap::new();
        pkg_names.insert("pkg".to_string(), "pkg".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

        // The top-level container should be "pkg::deep::nested"
        // (the common prefix), wrapped in "pkg::deep" and "pkg".
        // Since common_prefix "pkg::deep::nested" is longer than
        // pkg_display_name "pkg", we get wrapping.
        if let DragNode::Container { name, children, .. } = &tree {
            assert_eq!(name, "pkg");
            // Should have one child: "pkg::deep" container.
            assert_eq!(children.len(), 1);
            if let DragNode::Container { name, children, .. } = &children[0] {
                assert_eq!(name, "pkg::deep");
                assert_eq!(children.len(), 1);
                if let DragNode::Container { name, children, .. } = &children[0] {
                    assert_eq!(name, "pkg::deep::nested");
                    assert_eq!(children.len(), 2);
                    let names: Vec<&str> = children
                        .iter()
                        .map(|c| match c {
                            DragNode::Module { name, .. } => name.as_str(),
                            DragNode::Container { name, .. } => name.as_str(),
                        })
                        .collect();
                    assert!(names.contains(&"pkg::deep::nested::a"));
                    assert!(names.contains(&"pkg::deep::nested::b"));
                } else {
                    panic!("expected container at pkg::deep::nested");
                }
            } else {
                panic!("expected container at pkg::deep");
            }
        } else {
            panic!("expected root container");
        }
    }

    #[test]
    fn tree_module_moved_between_packages_pays_different_penalty() {
        // Moving a module from pkg_a (maint=0.5) to pkg_b (maint=1.0)
        // changes which penalty it pays.
        let modules_a = vec![mdl("pkg_a::mod1", 10.0)];
        let module_pkg_a = module_pkg_map(&[("pkg_a::mod1", "pkg_a")]);
        let pkg_maints_a = HashMap::from([("pkg_a".to_string(), 0.5)]);
        let pkg_names_a = HashMap::from([("pkg_a".to_string(), "pkg_a".to_string())]);

        let tree_a = build_drag_tree(
            &modules_a,
            &module_pkg_a,
            &pkg_maints_a,
            &pkg_names_a,
            2.5,
            false,
        );
        let mult_a = (2.5 * 0.5_f64).exp(); // exp(p * (1-0.5))

        // Same module under pkg_b (maint=1.0).
        let modules_b = vec![mdl("pkg_b::mod1", 10.0)];
        let module_pkg_b = module_pkg_map(&[("pkg_b::mod1", "pkg_b")]);
        let pkg_maints_b = HashMap::from([("pkg_b".to_string(), 1.0)]);
        let pkg_names_b = HashMap::from([("pkg_b".to_string(), "pkg_b".to_string())]);

        let tree_b = build_drag_tree(
            &modules_b,
            &module_pkg_b,
            &pkg_maints_b,
            &pkg_names_b,
            2.5,
            false,
        );
        let mult_b = (2.5 * 0.0_f64).exp(); // exp(p * (1-1.0)) = exp(0) = 1.0

        assert!((drag_of(&tree_a) - mult_a * 10.0).abs() < 1e-9);
        assert!((drag_of(&tree_b) - mult_b * 10.0).abs() < 1e-9);
        // The module under pkg_a (worse maint) should have higher drag.
        assert!(drag_of(&tree_a) > drag_of(&tree_b));
    }

    #[test]
    fn tree_missing_maint_defaults_to_neutral() {
        // When a package has no maint score (not in pkg_maints),
        // the fallback multiplier should be exp(p * 0.5), matching
        // the neutral 0.5 default.
        let modules = vec![mdl("missing_maint::m", 10.0)];
        let module_pkg = module_pkg_map(&[("missing_maint::m", "missing_maint")]);
        let pkg_maints: HashMap<String, f64> = HashMap::new(); // missing!
        let mut pkg_names = HashMap::new();
        pkg_names.insert("missing_maint".to_string(), "missing_maint".to_string());

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);
        let mult = (2.5 * 0.5_f64).exp();
        assert!((drag_of(&tree) - mult * 10.0).abs() < 1e-9);
    }

    #[test]
    fn tree_python_dotted_paths_build_hierarchy() {
        // Python module identifiers are dotted (pkg.core, pkg.sub.core).
        // They must produce a nested pkg.sub container, not stay flat.
        let modules = vec![mdl_py("pkg.core", 10.0), mdl_py("pkg.sub.core", 20.0)];
        let module_pkg = module_pkg_map_py(&[("pkg.core", "pkg"), ("pkg.sub.core", "pkg")]);
        let pkg_maints = HashMap::from([("pkg".to_string(), 1.0)]);
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

        // maint=1.0 ⇒ flat sum.
        assert!((drag_of(&tree) - 30.0).abs() < 1e-9);

        // The tree must contain a pkg.sub container.
        if let DragNode::Container { children, .. } = &tree {
            let sub = children
                .iter()
                .find(|c| matches!(c, DragNode::Container { name, .. } if name == "pkg.sub"));
            assert!(sub.is_some(), "expected pkg.sub container");
        } else {
            panic!("expected container root");
        }
    }

    #[test]
    fn tree_skip_maintainability_uses_multiplier_one() {
        // With maintainability skipped, no maint score is computed; the
        // tree must use a 1.0 multiplier and leave maint_score absent.
        let modules = vec![mdl("pkg::m", 10.0)];
        let module_pkg = module_pkg_map(&[("pkg::m", "pkg")]);
        let pkg_maints: HashMap<String, f64> = HashMap::new();
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, true);

        assert!((drag_of(&tree) - 10.0).abs() < 1e-9);
        if let DragNode::Container { maint_score, .. } = &tree {
            assert!(maint_score.is_none(), "maint_score should be absent");
        } else {
            panic!("expected container");
        }
    }

    #[test]
    fn tree_maint_score_on_package_not_subtree() {
        // Selecting pkg::deep::{a,b} shares prefix "pkg::deep". The package
        // maint score must stay on the outermost "pkg" container, not the
        // "pkg::deep" subtree.
        let modules = vec![mdl("pkg::deep::a", 1.0), mdl("pkg::deep::b", 2.0)];
        let module_pkg = module_pkg_map(&[("pkg::deep::a", "pkg"), ("pkg::deep::b", "pkg")]);
        let pkg_maints = HashMap::from([("pkg".to_string(), 0.5)]);
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);

        let tree = build_drag_tree(&modules, &module_pkg, &pkg_maints, &pkg_names, 2.5, false);

        if let DragNode::Container {
            name,
            maint_score,
            children,
            ..
        } = &tree
        {
            assert_eq!(name, "pkg");
            assert_eq!(*maint_score, Some(0.5));
            // Child should be "pkg::deep" with maint_score None.
            if let DragNode::Container {
                name, maint_score, ..
            } = &children[0]
            {
                assert_eq!(name, "pkg::deep");
                assert!(maint_score.is_none());
            } else {
                panic!("expected pkg::deep container");
            }
        } else {
            panic!("expected pkg container");
        }
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
