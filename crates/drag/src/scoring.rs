use std::collections::{HashMap, HashSet};

use crate::DragConfig;

/// Compute the multiplicative drag score for a module.
///
/// `drag = tokens^w1 · (1+deps)^w2 · (1 + k·(1−comp))`.
///
/// When comprehensibility is `None` (scoring skipped or failed), the
/// comprehensibility factor is treated as neutral (`1.0`).
pub(crate) fn compute_drag(
    config: &DragConfig,
    token_count: usize,
    deps: usize,
    comprehensibility: Option<f64>,
) -> f64 {
    let tokens_factor = (token_count as f64).powf(config.w1);
    let deps_factor = (1.0 + deps as f64).powf(config.w2);
    let comp_factor = match comprehensibility {
        Some(c) => 1.0 + config.k * (1.0 - c),
        None => 1.0,
    };

    tokens_factor * deps_factor * comp_factor
}

/// Compute `deps(m)` for every module: the sum of token counts of all
/// modules transitively reachable from `m`, excluding `m` itself.
///
/// Self-edges are ignored, cycles terminate naturally via the visited set,
/// and a token count of zero is used for any reachable module missing from
/// the token map.
pub(crate) fn compute_deps(
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

    let mut deps: HashMap<String, usize> = HashMap::new();
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

        let total = visited
            .iter()
            .map(|m| token_counts.get(m).copied().unwrap_or(0))
            .sum();
        deps.insert(node.clone(), total);
    }

    deps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DragConfig;

    #[test]
    fn compute_drag_uses_multiplicative_formula() {
        let config = DragConfig {
            w1: 1.0,
            w2: 0.5,
            k: 1.0,
        };
        // 100^1 * (1+3)^0.5 * (1 + 1*(1-0.5)) = 100 * 2 * 1.5 = 300
        let drag = compute_drag(&config, 100, 3, Some(0.5));
        assert!((drag - 300.0).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_treats_unscored_comprehensibility_as_neutral() {
        let config = DragConfig {
            w1: 1.0,
            w2: 0.5,
            k: 1.0,
        };
        // 100^1 * (1+3)^0.5 * 1.0 = 200
        let drag = compute_drag(&config, 100, 3, None);
        assert!((drag - 200.0).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_zero_tokens_is_zero() {
        let config = DragConfig {
            w1: 1.0,
            w2: 0.5,
            k: 1.0,
        };
        assert_eq!(compute_drag(&config, 0, 10, Some(0.5)), 0.0);
    }

    #[test]
    fn compute_drag_comp_factor_is_multiplicative() {
        // Sanity check that the comprehensibility factor is a multiplier,
        // not an additive term in log space.
        let config = DragConfig {
            w1: 1.0,
            w2: 0.0,
            k: 2.0,
        };
        let no_comp = compute_drag(&config, 10, 0, None);
        let with_comp = compute_drag(&config, 10, 0, Some(0.5));
        assert!((with_comp - no_comp * 2.0).abs() < 1e-12);
    }

    #[test]
    fn deps_transitive_diamond() {
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
        let deps = compute_deps(&edges, &tokens);
        assert_eq!(deps.get("a"), Some(&90));
        assert_eq!(deps.get("b"), Some(&40));
        assert_eq!(deps.get("c"), Some(&40));
        assert_eq!(deps.get("d"), Some(&0));
    }

    #[test]
    fn deps_cycle_stops_at_visited_set() {
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
        let deps = compute_deps(&edges, &tokens);
        assert_eq!(deps.get("a"), Some(&50));
        assert_eq!(deps.get("b"), Some(&40));
        assert_eq!(deps.get("c"), Some(&30));
    }

    #[test]
    fn deps_self_edges_ignored() {
        let edges = vec![
            ("a".to_string(), "a".to_string()),
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "b".to_string()),
        ];
        let tokens = HashMap::from([("a".to_string(), 10usize), ("b".to_string(), 20usize)]);
        let deps = compute_deps(&edges, &tokens);
        assert_eq!(deps.get("a"), Some(&20));
        assert_eq!(deps.get("b"), Some(&0));
    }

    #[test]
    fn deps_counts_out_of_target_modules() {
        // `b` is not part of the scored target set, but its token count is
        // still part of the transitive footprint for `a`.
        let edges = vec![("a".to_string(), "b".to_string())];
        let tokens = HashMap::from([("a".to_string(), 10usize), ("b".to_string(), 25usize)]);
        let deps = compute_deps(&edges, &tokens);
        assert_eq!(deps.get("a"), Some(&25));
    }
}
