#[cfg(test)]
pub(crate) fn git(dir: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("git should be available");
    assert!(status.success(), "git {:?} failed", args);
}