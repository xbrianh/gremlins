use std::collections::{HashMap, HashSet};

use crate::DragConfig;

/// Compute the multiplicative drag score for a module.
///
/// `drag = (1+footprint)^w2 · exp(m·(1−comp))`.
///
/// `footprint` is the module's own token count plus the token count of every
/// module transitively reachable from it (see [`compute_footprint`]).
///
/// When comprehensibility is `None` (scoring skipped or failed), the
/// comprehensibility factor is treated as neutral (`1.0`).
pub(crate) fn compute_drag(
    config: &DragConfig,
    footprint: usize,
    comprehensibility: Option<f64>,
) -> f64 {
    let footprint_factor = (1.0 + footprint as f64).powf(config.w2);
    let comp_factor = match comprehensibility {
        Some(c) => (config.m * (1.0 - c)).exp(),
        None => 1.0,
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DragConfig;

    #[test]
    fn compute_drag_uses_multiplicative_formula() {
        let config = DragConfig { w2: 0.5, m: 1.0 };
        // (1+3)^0.5 * exp(1.0*(1-0.5)) = 2 * exp(0.5) ≈ 3.2974
        let drag = compute_drag(&config, 3, Some(0.5));
        let expected = 2.0 * 0.5_f64.exp();
        assert!((drag - expected).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_treats_unscored_comprehensibility_as_neutral() {
        let config = DragConfig { w2: 0.5, m: 1.0 };
        // (1+3)^0.5 * exp(1.0*(1-1.0)) = 2 * exp(0) = 2.0
        let drag = compute_drag(&config, 3, None);
        assert!((drag - 2.0).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_zero_footprint_with_comp() {
        let config = DragConfig { w2: 0.5, m: 1.0 };
        // (1+0)^0.5 * exp(1.0*(1-0.5)) = 1 * exp(0.5) ≈ 1.6487
        let drag = compute_drag(&config, 0, Some(0.5));
        assert!((drag - 0.5_f64.exp()).abs() < 1e-12);
    }

    #[test]
    fn compute_drag_comp_factor_is_multiplicative() {
        // Sanity check that the comprehensibility factor is a multiplier,
        // not an additive term in log space.
        let config = DragConfig { w2: 0.0, m: 2.0 };
        // (1+10)^0 * exp(2.0*(1-1.0)) = exp(0) = 1.0
        let no_comp = compute_drag(&config, 10, None);
        // (1+10)^0 * exp(2.0*(1-0.5)) = exp(1.0) ≈ 2.718
        let with_comp = compute_drag(&config, 10, Some(0.5));
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
}
