use std::path::Path;

/// Abstract landscape exploration, decoupled from `crate::landscape` so
/// the `toolbox` crate does not pull the concrete landscape implementation
/// into every downstream footprint.
pub(crate) trait LandscapeExplorerFn: Send + Sync {
    /// Explore a directory tree and return the formatted landscape block.
    fn explore_directory(
        &self,
        path: &Path,
        depth_limit: usize,
        token_budget: Option<usize>,
    ) -> Result<String, String>;

    /// Outline a single file and return the formatted result.
    fn explore_file(&self, path: &Path) -> Result<String, String>;
}
