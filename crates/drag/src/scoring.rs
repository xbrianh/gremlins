use std::collections::{BTreeMap, HashMap, HashSet};

use crate::maint::DirMaintResult;

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
        ci_lower: Option<f64>,
        ci_upper: Option<f64>,
        drag: f64,
    },
    Container {
        name: String,
        /// Every container carries its own maintainability score (None
        /// only when maintainability is skipped or for the synthetic
        /// workspace/anchor root).
        maint_score: Option<f64>,
        maint_ci_lower: Option<f64>,
        maint_ci_upper: Option<f64>,
        drag: f64,
        children: Vec<DragNode>,
    },
}

// ---------------------------------------------------------------------------
// Tree construction & drag computation
// ---------------------------------------------------------------------------

/// Build a drag tree from scored modules, keyed by *directory segments*
/// derived from each module's source file.
///
/// `module_dir_segments` maps each (module_path, file) pair to the
/// sequence of directory names between the package's source root and the
/// file's parent directory.  An empty vec means the file lives directly
/// in the source root.
///
/// `dir_maints` maps the *absolute path* of each scored directory to its
/// maintainability score (0–1).  `pkg_src_dirs` maps each package key to
/// its source-root directory, used to resolve absolute paths during
/// trie→tree conversion.
///
/// Orphan modules (no package) are placed under a synthetic root and use
/// their module-path segments as before.
///
/// The tree is walked bottom-up so every container's `drag` field is the
/// value of the recursive formula `drag(C) = exp(p·(1−maint(C)))·(sum of
/// direct module drags) + (sum of child container drags)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_drag_tree(
    modules: &[super::ModuleDrag],
    module_pkg: &HashMap<(String, String), String>,
    module_dir_segments: &HashMap<(String, String), Vec<String>>,
    dir_maints: &HashMap<String, DirMaintResult>,
    skipped_dirs: &HashSet<String>,
    pkg_name_for_key: &HashMap<String, String>,
    pkg_src_dirs: &HashMap<String, std::path::PathBuf>,
    p: f64,
    skip_maintainability: bool,
) -> Result<DragNode, String> {
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

    // ── 2. Build one container per package, using directory trie ──
    let mut package_containers: Vec<DragNode> = Vec::new();

    for (pkg_key, indices) in &pkg_module_indices {
        let pkg_display_name = pkg_name_for_key
            .get(pkg_key)
            .cloned()
            .unwrap_or_else(|| pkg_key.clone());
        // Determine separator from the package's module paths.
        let module_paths: Vec<&str> = indices
            .iter()
            .map(|&i| modules[i].module.as_str())
            .collect();
        let sep = path_separator(&module_paths);

        let src_dir = pkg_src_dirs
            .get(pkg_key)
            .cloned()
            .expect("pkg_src_dirs must contain every package key");

        // Build a directory trie for this package's modules.
        let mut dir_trie = DirTrieNode::default();
        for &idx in indices {
            let key = (modules[idx].module.clone(), modules[idx].file.clone());
            let segments = module_dir_segments
                .get(&key)
                .map(|v| v.as_slice())
                .unwrap_or(&[]);
            dir_trie.insert(segments, idx);
        }

        let container = dir_trie_to_drag_node(
            &dir_trie,
            modules,
            &src_dir,
            "",
            pkg_display_name,
            dir_maints,
            skipped_dirs,
            p,
            skip_maintainability,
            sep,
        )?;

        package_containers.push(container);
    }

    // ── 3. Handle orphan modules (no package) ───────────────────────
    let orphan_nodes = if orphan_indices.is_empty() {
        Vec::new()
    } else {
        let mut orphan_trie = TrieNode::default();
        for &idx in &orphan_indices {
            let segments = module_segments(&modules[idx].module);
            orphan_trie.insert(&segments, idx);
        }
        let orphan_sep = path_separator(&[modules[orphan_indices[0]].module.as_str()]);
        let orphan_root = trie_to_drag_node(&orphan_trie, modules, String::new(), 1.0, orphan_sep);
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
        return Ok(all_children.remove(0));
    }

    let total_drag: f64 = all_children.iter().map(drag_of).sum();
    Ok(DragNode::Container {
        name: String::new(),
        maint_score: None,
        maint_ci_lower: None,
        maint_ci_upper: None,
        drag: total_drag,
        children: all_children,
    })
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

/// Trie for orphan modules (no package). Keyed by module-path segments.
#[derive(Default)]
struct TrieNode {
    module_indices: Vec<usize>,
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

/// Trie for package modules, keyed by directory-path segments (relative
/// to the package source root).
#[derive(Default)]
struct DirTrieNode {
    module_indices: Vec<usize>,
    children: BTreeMap<String, DirTrieNode>,
}

impl DirTrieNode {
    fn insert(&mut self, segments: &[String], module_idx: usize) {
        if segments.is_empty() {
            self.module_indices.push(module_idx);
            return;
        }
        let child = self.children.entry(segments[0].clone()).or_default();
        child.insert(&segments[1..], module_idx);
    }
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

/// Recursively convert a directory trie into a [`DragNode`] tree, computing
/// container drag bottom-up.
///
/// `src_dir` is the absolute path of the package's source root, used to
/// resolve per-directory maintainability scores from `dir_maints`.
/// `rel_dir` is the directory path relative to `src_dir` (empty for the
/// source root itself). `display_name` is what appears in the tree.
#[allow(clippy::too_many_arguments)]
fn dir_trie_to_drag_node(
    trie: &DirTrieNode,
    modules: &[super::ModuleDrag],
    src_dir: &std::path::Path,
    rel_dir: &str,
    display_name: String,
    dir_maints: &HashMap<String, DirMaintResult>,
    skipped_dirs: &HashSet<String>,
    p: f64,
    skip_maintainability: bool,
    sep: &str,
) -> Result<DragNode, String> {
    // Build the absolute directory path for this trie node.
    let abs_dir = if rel_dir.is_empty() {
        src_dir.to_path_buf()
    } else {
        src_dir.join(rel_dir)
    };
    let abs_dir_str = abs_dir.to_string_lossy().into_owned();

    // Intentionally skipped intermediate directories (no direct source
    // files, e.g. `pkg/a` when only `pkg/a/b/c.py` exists) are structural
    // containers with no maintainability penalty.
    let is_skipped = skipped_dirs.contains(&abs_dir_str);

    let (maint_score, maint_ci_lower, maint_ci_upper, maint_mult) =
        if skip_maintainability || is_skipped {
            (None, None, None, 1.0)
        } else {
            let r = dir_maints.get(&abs_dir_str).ok_or_else(|| {
                format!(
                    "maintainability score missing for directory '{abs_dir_str}' — \
                     scoring was enabled but this directory was not scored. \
                     This indicates a mismatch between the maintainability \
                     scoring pass and the directory trie; check whether the \
                     directory was excluded from scoring_dirs."
                )
            })?;
            (Some(r.score), Some(r.ci_lower), Some(r.ci_upper), (p * (1.0 - r.score)).exp())
        };

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
                ci_lower: m.ci_lower,
                ci_upper: m.ci_upper,
                drag: m.drag,
            }
        })
        .collect();

    // Child containers (subtries that have children of their own).
    let child_containers: Vec<DragNode> = trie
        .children
        .iter()
        .map(|(seg, child_trie)| {
            let child_rel_path = if rel_dir.is_empty() {
                seg.clone()
            } else {
                format!("{rel_dir}/{seg}")
            };
            let child_name = if display_name.is_empty() {
                seg.clone()
            } else {
                format!("{display_name}{sep}{seg}")
            };
            dir_trie_to_drag_node(
                child_trie,
                modules,
                src_dir,
                &child_rel_path,
                child_name,
                dir_maints,
                skipped_dirs,
                p,
                skip_maintainability,
                sep,
            )
        })
        .collect::<Result<Vec<_>, String>>()?;

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
    // sort by drag descending.
    all_children.sort_by(|a, b| {
        let a_is_module = matches!(a, DragNode::Module { .. });
        let b_is_module = matches!(b, DragNode::Module { .. });
        match (a_is_module, b_is_module) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                let a_drag = drag_of(a);
                let b_drag = drag_of(b);
                b_drag
                    .partial_cmp(&a_drag)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| match (a, b) {
                        (DragNode::Module { name: na, .. }, DragNode::Module { name: nb, .. }) => {
                            na.cmp(nb)
                        }
                        (
                            DragNode::Container { name: na, .. },
                            DragNode::Container { name: nb, .. },
                        ) => na.cmp(nb),
                        _ => std::cmp::Ordering::Equal,
                    })
            }
        }
    });

    Ok(DragNode::Container {
        name: display_name,
        maint_score,
        maint_ci_lower,
        maint_ci_upper,
        drag,
        children: all_children,
    })
}

/// Recursively convert a module-path trie (orphans) into a [`DragNode`]
/// tree.  Orphans never get maintainability.
fn trie_to_drag_node(
    trie: &TrieNode,
    modules: &[super::ModuleDrag],
    name: String,
    maint_mult: f64,
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
                ci_lower: m.ci_lower,
                ci_upper: m.ci_upper,
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
            trie_to_drag_node(child_trie, modules, child_name, maint_mult, sep)
        })
        .collect();

    // Flatten containers with at most one direct module and no child
    // subtrees.  Orphans never have maintainability so there is no
    // penalty to preserve.
    if trie.children.is_empty() && direct_modules.len() <= 1 {
        if let Some(m) = direct_modules.into_iter().next() {
            return m;
        }
        // Empty: return a trivial container.
        return DragNode::Container {
            name,
            maint_score: None,
            maint_ci_lower: None,
            maint_ci_upper: None,
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
        maint_ci_lower: None,
        maint_ci_upper: None,
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

    // ── Drag tree tests (directory trie) ──────────────────────────

    /// Helper to make a ModuleDrag with the given fields.
    fn mdl(name: &str, file: &str, drag: f64) -> super::super::ModuleDrag {
        super::super::ModuleDrag {
            module: name.to_string(),
            file: file.to_string(),
            token_count: 100,
            footprint: 0,
            comprehensibility: Some(0.5),
            ci_lower: Some(0.5),
            ci_upper: Some(0.5),
            drag,
        }
    }

    /// Build a `build_drag_tree` call for Rust-style modules.
    #[allow(clippy::too_many_arguments)]
    fn build_rust_tree(
        modules: &[super::super::ModuleDrag],
        module_pkg: &HashMap<(String, String), String>,
        dir_segments: &HashMap<(String, String), Vec<String>>,
        dir_maints: &HashMap<String, DirMaintResult>,
        skipped_dirs: &HashSet<String>,
        pkg_names: &HashMap<String, String>,
        src_dirs: &HashMap<String, std::path::PathBuf>,
        p: f64,
        skip: bool,
    ) -> Result<DragNode, String> {
        build_drag_tree(
            modules,
            module_pkg,
            dir_segments,
            dir_maints,
            skipped_dirs,
            pkg_names,
            src_dirs,
            p,
            skip,
        )
    }

    /// Build a DirMaintResult from a score, with degenerate CIs (score, score, score).
    fn maint_result(score: f64) -> DirMaintResult {
        DirMaintResult {
            score,
            ci_lower: score,
            ci_upper: score,
        }
    }

    /// Build dir_maints map from f64 scores (for test convenience).
    fn dir_maints_map<const N: usize>(entries: [(&str, f64); N]) -> HashMap<String, DirMaintResult> {
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), maint_result(v)))
            .collect()
    }

    #[test]
    fn tree_flat_sum_with_maint_one() {
        let src = std::path::PathBuf::from("/src");
        let modules = vec![
            mdl("gremlins::config", "/src/config.rs", 10.0),
            mdl("gremlins::clients::client", "/src/clients/client.rs", 5.0),
            mdl(
                "gremlins::clients::provider",
                "/src/clients/provider.rs",
                7.0,
            ),
            mdl(
                "gremlins::executor::gremlin",
                "/src/executor/gremlin.rs",
                20.0,
            ),
        ];
        let module_pkg = HashMap::from([
            (
                ("gremlins::config".to_string(), "/src/config.rs".to_string()),
                "gremlins".to_string(),
            ),
            (
                (
                    "gremlins::clients::client".to_string(),
                    "/src/clients/client.rs".to_string(),
                ),
                "gremlins".to_string(),
            ),
            (
                (
                    "gremlins::clients::provider".to_string(),
                    "/src/clients/provider.rs".to_string(),
                ),
                "gremlins".to_string(),
            ),
            (
                (
                    "gremlins::executor::gremlin".to_string(),
                    "/src/executor/gremlin.rs".to_string(),
                ),
                "gremlins".to_string(),
            ),
        ]);
        let dir_segments = HashMap::from([
            (
                ("gremlins::config".to_string(), "/src/config.rs".to_string()),
                vec![],
            ),
            (
                (
                    "gremlins::clients::client".to_string(),
                    "/src/clients/client.rs".to_string(),
                ),
                vec!["clients".to_string()],
            ),
            (
                (
                    "gremlins::clients::provider".to_string(),
                    "/src/clients/provider.rs".to_string(),
                ),
                vec!["clients".to_string()],
            ),
            (
                (
                    "gremlins::executor::gremlin".to_string(),
                    "/src/executor/gremlin.rs".to_string(),
                ),
                vec!["executor".to_string()],
            ),
        ]);
        let dir_maints = dir_maints_map([
            ("/src", 1.0),
            ("/src/clients", 1.0),
            ("/src/executor", 1.0),
        ]);
        let pkg_names = HashMap::from([("gremlins".to_string(), "gremlins".to_string())]);
        let src_dirs = HashMap::from([("gremlins".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        let flat_sum: f64 = modules.iter().map(|m| m.drag).sum();
        assert!((drag_of(&tree) - flat_sum).abs() < 1e-9);
    }

    #[test]
    fn tree_directory_penalty_applies_to_immediate_modules_only() {
        // a/ (maint 0.5) contains a/b/ (maint 1.0) which contains one module.
        // The module pays only its immediate parent's penalty (1.0 => 1.0),
        // and a/'s 0.5 penalty is applied once at the a/ level.
        let src = std::path::PathBuf::from("/pkg/src");
        let modules = vec![mdl("pkg::a::b", "/pkg/src/a/b.rs", 10.0)];
        let module_pkg = HashMap::from([(
            ("pkg::a::b".to_string(), "/pkg/src/a/b.rs".to_string()),
            "pkg".to_string(),
        )]);
        let dir_segments = HashMap::from([(
            ("pkg::a::b".to_string(), "/pkg/src/a/b.rs".to_string()),
            vec!["a".to_string()],
        )]);
        let dir_maints = dir_maints_map([
            ("/pkg/src", 0.5),
            ("/pkg/src/a", 1.0),
        ]);
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        // a/ (maint 1.0) contains one module with drag 10.0, so:
        //   drag(a) = exp(p·(1−1.0)) · 10 = 10
        // pkg (maint 0.5) has no direct modules, so its penalty only
        // affects its own direct module children (none). Its drag is the
        // summed child drag: drag(pkg) = drag(a) = 10.
        // This preserves the "exactly one penalty per module" invariant:
        // the module paid only a/'s score, and a/'s drag is summed into pkg.
        assert!((drag_of(&tree) - 10.0).abs() < 1e-9);

        // Verify each container carries its own score.
        if let DragNode::Container {
            name,
            maint_score,
            children,
            ..
        } = &tree
        {
            assert_eq!(name, "pkg");
            assert_eq!(*maint_score, Some(0.5));
            assert_eq!(children.len(), 1);
            if let DragNode::Container {
                name, maint_score, ..
            } = &children[0]
            {
                assert_eq!(name, "pkg::a");
                assert_eq!(*maint_score, Some(1.0));
            } else {
                panic!("expected pkg::a container");
            }
        } else {
            panic!("expected pkg container");
        }
    }

    #[test]
    fn tree_rust_files_produce_directory_layout() {
        // src/lib.rs + src/foo/bar.rs + src/foo/baz.rs produce
        // root (package name) -> foo -> two module leaves; no synthetic
        // foo::bar container for the file.
        let src = std::path::PathBuf::from("/pkg/src");
        let modules = vec![
            mdl("pkg", "/pkg/src/lib.rs", 1.0),
            mdl("pkg::foo::bar", "/pkg/src/foo/bar.rs", 2.0),
            mdl("pkg::foo::baz", "/pkg/src/foo/baz.rs", 3.0),
        ];
        let module_pkg = HashMap::from([
            (
                ("pkg".to_string(), "/pkg/src/lib.rs".to_string()),
                "pkg".to_string(),
            ),
            (
                (
                    "pkg::foo::bar".to_string(),
                    "/pkg/src/foo/bar.rs".to_string(),
                ),
                "pkg".to_string(),
            ),
            (
                (
                    "pkg::foo::baz".to_string(),
                    "/pkg/src/foo/baz.rs".to_string(),
                ),
                "pkg".to_string(),
            ),
        ]);
        let dir_segments = HashMap::from([
            (("pkg".to_string(), "/pkg/src/lib.rs".to_string()), vec![]),
            (
                (
                    "pkg::foo::bar".to_string(),
                    "/pkg/src/foo/bar.rs".to_string(),
                ),
                vec!["foo".to_string()],
            ),
            (
                (
                    "pkg::foo::baz".to_string(),
                    "/pkg/src/foo/baz.rs".to_string(),
                ),
                vec!["foo".to_string()],
            ),
        ]);
        let dir_maints = dir_maints_map([
            ("/pkg/src", 1.0),
            ("/pkg/src/foo", 1.0),
        ]);
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        if let DragNode::Container { name, children, .. } = &tree {
            assert_eq!(name, "pkg");
            assert_eq!(children.len(), 2); // pkg module + foo container
            let foo = children
                .iter()
                .find(|c| matches!(c, DragNode::Container { name, .. } if name == "pkg::foo"))
                .expect("expected pkg::foo container");
            if let DragNode::Container { children, .. } = foo {
                assert_eq!(children.len(), 2);
                assert!(children
                    .iter()
                    .all(|c| matches!(c, DragNode::Module { .. })));
            } else {
                panic!("expected pkg::foo container");
            }
        } else {
            panic!("expected pkg container");
        }
    }

    #[test]
    fn tree_rust_mod_rs_attaches_to_its_directory() {
        // src/foo/mod.rs attaches to the `foo` container, not a deeper
        // `foo::mod` level.
        let src = std::path::PathBuf::from("/pkg/src");
        let modules = vec![mdl("pkg::foo", "/pkg/src/foo/mod.rs", 10.0)];
        let module_pkg = HashMap::from([(
            ("pkg::foo".to_string(), "/pkg/src/foo/mod.rs".to_string()),
            "pkg".to_string(),
        )]);
        let dir_segments = HashMap::from([(
            ("pkg::foo".to_string(), "/pkg/src/foo/mod.rs".to_string()),
            vec!["foo".to_string()],
        )]);
        let dir_maints = dir_maints_map([
            ("/pkg/src", 1.0),
            ("/pkg/src/foo", 1.0),
        ]);
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        if let DragNode::Container { name, children, .. } = &tree {
            assert_eq!(name, "pkg");
            assert_eq!(children.len(), 1);
            if let DragNode::Container { name, children, .. } = &children[0] {
                assert_eq!(name, "pkg::foo");
                assert_eq!(children.len(), 1);
                assert!(
                    matches!(&children[0], DragNode::Module { name, .. } if name == "pkg::foo")
                );
            } else {
                panic!("expected pkg::foo container");
            }
        } else {
            panic!("expected pkg container");
        }
    }

    #[test]
    fn tree_multi_package_workspace_root() {
        let src_a = std::path::PathBuf::from("/ws/pkg_a/src");
        let src_b = std::path::PathBuf::from("/ws/pkg_b/src");
        let modules = vec![
            mdl("pkg_a::mod1", "/ws/pkg_a/src/mod1.rs", 10.0),
            mdl("pkg_a::mod2", "/ws/pkg_a/src/mod2.rs", 20.0),
            mdl("pkg_b::mod1", "/ws/pkg_b/src/mod1.rs", 30.0),
        ];
        let module_pkg = HashMap::from([
            (
                (
                    "pkg_a::mod1".to_string(),
                    "/ws/pkg_a/src/mod1.rs".to_string(),
                ),
                "pkg_a".to_string(),
            ),
            (
                (
                    "pkg_a::mod2".to_string(),
                    "/ws/pkg_a/src/mod2.rs".to_string(),
                ),
                "pkg_a".to_string(),
            ),
            (
                (
                    "pkg_b::mod1".to_string(),
                    "/ws/pkg_b/src/mod1.rs".to_string(),
                ),
                "pkg_b".to_string(),
            ),
        ]);
        let dir_segments = HashMap::from([
            (
                (
                    "pkg_a::mod1".to_string(),
                    "/ws/pkg_a/src/mod1.rs".to_string(),
                ),
                vec![],
            ),
            (
                (
                    "pkg_a::mod2".to_string(),
                    "/ws/pkg_a/src/mod2.rs".to_string(),
                ),
                vec![],
            ),
            (
                (
                    "pkg_b::mod1".to_string(),
                    "/ws/pkg_b/src/mod1.rs".to_string(),
                ),
                vec![],
            ),
        ]);
        let dir_maints = dir_maints_map([
            ("/ws/pkg_a/src", 1.0),
            ("/ws/pkg_b/src", 1.0),
        ]);
        let pkg_names = HashMap::from([
            ("pkg_a".to_string(), "pkg_a".to_string()),
            ("pkg_b".to_string(), "pkg_b".to_string()),
        ]);
        let src_dirs = HashMap::from([("pkg_a".to_string(), src_a), ("pkg_b".to_string(), src_b)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        assert!(matches!(tree, DragNode::Container { .. }));
        assert!((drag_of(&tree) - 60.0).abs() < 1e-9);
    }

    #[test]
    fn tree_single_module_orphan_returns_it_directly() {
        let modules = vec![mdl("lone_mod", "/lone_mod.rs", 42.0)];
        let tree = build_rust_tree(
            &modules,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            2.5,
            false,
        )
        .unwrap();
        assert!(matches!(tree, DragNode::Module { .. }));
        assert!((drag_of(&tree) - 42.0).abs() < 1e-9);
    }

    #[test]
    fn tree_container_ci_fields_preserved() {
        // CI fields are propagated from DirMaintResult into DragNode::Container.
        let src = std::path::PathBuf::from("/pkg/src");
        let modules = vec![mdl("pkg::m", "/pkg/src/m.rs", 10.0)];
        let module_pkg = HashMap::from([(
            ("pkg::m".to_string(), "/pkg/src/m.rs".to_string()),
            "pkg".to_string(),
        )]);
        let dir_segments = HashMap::from([(
            ("pkg::m".to_string(), "/pkg/src/m.rs".to_string()),
            vec![],
        )]);
        let mut dir_maints: HashMap<String, DirMaintResult> = HashMap::new();
        dir_maints.insert(
            "/pkg/src".to_string(),
            DirMaintResult {
                score: 0.75,
                ci_lower: 0.65,
                ci_upper: 0.85,
            },
        );
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        if let DragNode::Container {
            maint_score,
            maint_ci_lower,
            maint_ci_upper,
            ..
        } = &tree
        {
            assert_eq!(*maint_score, Some(0.75));
            assert_eq!(*maint_ci_lower, Some(0.65));
            assert_eq!(*maint_ci_upper, Some(0.85));
        } else {
            panic!("expected Container");
        }
    }

    #[test]
    fn tree_orphan_modules_nested_by_path() {
        let modules = vec![
            mdl("orphan::a", "/orphan/a.rs", 10.0),
            mdl("orphan::b::c", "/orphan/b/c.rs", 20.0),
        ];
        let tree = build_rust_tree(
            &modules,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
            &HashMap::new(),
            &HashMap::new(),
            2.5,
            false,
        )
        .unwrap();
        assert!(matches!(tree, DragNode::Container { .. }));
        if let DragNode::Container { name, children, .. } = &tree {
            assert_eq!(name, "orphan");
            assert_eq!(children.len(), 2);
        }
    }

    #[test]
    fn tree_skip_maintainability_uses_multiplier_one() {
        let src = std::path::PathBuf::from("/pkg/src");
        let modules = vec![mdl("pkg::m", "/pkg/src/m.rs", 10.0)];
        let module_pkg = HashMap::from([(
            ("pkg::m".to_string(), "/pkg/src/m.rs".to_string()),
            "pkg".to_string(),
        )]);
        let dir_segments =
            HashMap::from([(("pkg::m".to_string(), "/pkg/src/m.rs".to_string()), vec![])]);
        let dir_maints: HashMap<String, DirMaintResult> = HashMap::new();
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            true,
        )
        .unwrap();

        assert!((drag_of(&tree) - 10.0).abs() < 1e-9);
        if let DragNode::Container { maint_score, .. } = &tree {
            assert!(maint_score.is_none(), "maint_score should be absent");
        } else {
            panic!("expected container");
        }
    }

    #[test]
    fn tree_python_dotted_paths_build_nested_containers() {
        // Python packages use `.` separator; verify containers are named
        // with dots.
        let src = std::path::PathBuf::from("/pkg");
        let modules = vec![
            mdl("pkg.core", "/pkg/core.py", 10.0),
            mdl("pkg.sub.core", "/pkg/sub/core.py", 20.0),
        ];
        let module_pkg = HashMap::from([
            (
                ("pkg.core".to_string(), "/pkg/core.py".to_string()),
                "pkg".to_string(),
            ),
            (
                ("pkg.sub.core".to_string(), "/pkg/sub/core.py".to_string()),
                "pkg".to_string(),
            ),
        ]);
        let dir_segments = HashMap::from([
            (("pkg.core".to_string(), "/pkg/core.py".to_string()), vec![]),
            (
                ("pkg.sub.core".to_string(), "/pkg/sub/core.py".to_string()),
                vec!["sub".to_string()],
            ),
        ]);
        let dir_maints = dir_maints_map([("/pkg", 1.0), ("/pkg/sub", 1.0)]);
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        )
        .unwrap();

        // maint=1.0 ⇒ flat sum.
        assert!((drag_of(&tree) - 30.0).abs() < 1e-9);

        // Must contain a pkg.sub container.
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
    fn tree_missing_dir_maint_is_an_error() {
        // When maintainability scoring is enabled and a directory has no
        // score in dir_maints, build_drag_tree must return an error instead
        // of silently defaulting to the neutral 0.5.
        let src = std::path::PathBuf::from("/pkg/src");
        let modules = vec![mdl("pkg::m", "/pkg/src/m.rs", 10.0)];
        let module_pkg = HashMap::from([(
            ("pkg::m".to_string(), "/pkg/src/m.rs".to_string()),
            "pkg".to_string(),
        )]);
        let dir_segments =
            HashMap::from([(("pkg::m".to_string(), "/pkg/src/m.rs".to_string()), vec![])]);
        // dir_maints is empty — neither /pkg/src nor any sub-dir has a score.
        let dir_maints: HashMap<String, DirMaintResult> = HashMap::new();
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let result = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &HashSet::new(),
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        );

        assert!(
            result.is_err(),
            "expected error for missing maint score, got Ok"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("maintainability score missing"),
            "unexpected error message: {err}"
        );
        assert!(
            err.contains("/pkg/src"),
            "error should mention the missing directory: {err}"
        );
    }

    #[test]
    fn tree_skipped_intermediate_dir_is_container_not_error() {
        // When `pkg/a` is enumerated in scoring_dirs but has no direct
        // source files (all source lives in `pkg/a/b/`), the maint pass
        // marks it in `skipped_dirs`.  The trie still contains a node for
        // `a/` as an intermediate ancestor of `pkg::a::b::mod`, and
        // `dir_trie_to_drag_node` must treat it as a structural container
        // (no maint_score, no penalty) rather than erroring.
        let src = std::path::PathBuf::from("/pkg");
        let modules = vec![mdl("pkg.a.b.mod", "/pkg/a/b/mod.py", 10.0)];
        let module_pkg = HashMap::from([(
            ("pkg.a.b.mod".to_string(), "/pkg/a/b/mod.py".to_string()),
            "pkg".to_string(),
        )]);
        let dir_segments = HashMap::from([(
            ("pkg.a.b.mod".to_string(), "/pkg/a/b/mod.py".to_string()),
            vec!["a".to_string(), "b".to_string()],
        )]);
        // Only /pkg and /pkg/a/b are scored; /pkg/a is intentionally skipped.
        let dir_maints = dir_maints_map([("/pkg", 0.9), ("/pkg/a/b", 0.7)]);
        let skipped: HashSet<String> = ["/pkg/a".to_string()].into_iter().collect();
        let pkg_names = HashMap::from([("pkg".to_string(), "pkg".to_string())]);
        let src_dirs = HashMap::from([("pkg".to_string(), src)]);

        let tree = build_rust_tree(
            &modules,
            &module_pkg,
            &dir_segments,
            &dir_maints,
            &skipped,
            &pkg_names,
            &src_dirs,
            2.5,
            false,
        );
        assert!(
            tree.is_ok(),
            "skipped dir should not error: {:?}",
            tree.err()
        );

        let tree = tree.unwrap();
        // pkg container should have maint_score=Some(0.9)
        if let DragNode::Container {
            name,
            maint_score,
            children,
            ..
        } = &tree
        {
            assert_eq!(name, "pkg");
            assert_eq!(*maint_score, Some(0.9));
            // pkg::a should be a container with maint_score=None (skipped)
            assert_eq!(children.len(), 1);
            if let DragNode::Container {
                name,
                maint_score,
                children,
                ..
            } = &children[0]
            {
                assert_eq!(name, "pkg.a");
                assert_eq!(*maint_score, None);
                // pkg::a::b should contain the module
                assert_eq!(children.len(), 1);
                if let DragNode::Container {
                    name, maint_score, ..
                } = &children[0]
                {
                    assert_eq!(name, "pkg.a.b");
                    assert_eq!(*maint_score, Some(0.7));
                } else {
                    panic!("expected pkg.a.b container");
                }
            } else {
                panic!("expected pkg.a container");
            }
        } else {
            panic!("expected pkg container");
        }
    }
}
