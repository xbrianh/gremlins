# Review fixes

## maint.rs, lines 317–340 → lines 348–350
**Category**: Correctness / Readability  
**What changed**: Extracted `compute_maint_ci()` helper that reuses `compute_maint_score_from_map` for each per-estimate score, so the CI formula is identical to the point-estimate formula. Missing dimensions in an estimate now default to the neutral 2.0 (matching the `dim_means` aggregation), eliminating the downward bias from the old code that treated missing dimensions as zero. The inline duplicate scoring formula is gone — one definition in `compute_maint_score_from_map` now serves both.

## maint.rs, new function `compute_maint_ci` (lines 57–86)
**Category**: Correctness  
**What changed**: Guard against empty `per_estimate_scores` — returns `(point, point)` (degenerate CI at the point estimate) instead of calling `mean_with_ci` with an empty slice, which would panic.

## maint.rs, lines 370–375 (deleted) / line 350 (new)
**Category**: Correctness  
**What changed**: CI bounds are now clamped to `[0.0, 1.0]` inside `compute_maint_ci` so the displayed interval never exceeds the valid score range. This applies to both the `eprintln!` at line 352 and the human table output.

## main.rs, lines 1584–1590
**Category**: Correctness  
**What changed**: Human-table CI bounds are now clamped to `[0.0, 1.0]` at the formatting site (`lo.clamp(0.0, 1.0)` / `hi.clamp(0.0, 1.0)`) as a double-safety layer.

## maint.rs, tests (lines 790–888)
**Category**: Testing  
**What changed**: Added 5 tests for `compute_maint_ci`:
- `maint_ci_all_estimates_identical` — identical estimates → degenerate CI containing the point estimate
- `maint_ci_missing_dimension_defaults_to_neutral` — missing dimension uses neutral 2.0, CI contains point
- `maint_ci_single_estimate` — single estimate → degenerate CI
- `maint_ci_empty_estimate_batch` — empty batch → returns point as bounds
- `maint_ci_bounds_clamped_to_0_1` — wide variance doesn't push bounds outside [0, 1]