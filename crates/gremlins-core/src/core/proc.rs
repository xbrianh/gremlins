use std::io;
use std::path::Path;
use std::process::Command;

pub fn run_ok(cmd: &[String], cwd: Option<&Path>) -> Result<bool, io::Error> {
    if cmd.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty command"));
    }
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    if let Some(dir) = cwd {
        c.current_dir(dir);
    }
    let status = c.status()?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_ok_success() {
        assert!(run_ok(&["true".to_string()], None).unwrap());
    }

    #[test]
    fn test_run_ok_failure() {
        assert!(!run_ok(&["false".to_string()], None).unwrap());
    }

    #[test]
    fn test_run_ok_missing_command() {
        let err = run_ok(&["_nonexistent_command_xyzzy_".to_string()], None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn test_run_ok_empty_cmd() {
        let err = run_ok(&[], None).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_run_ok_with_cwd() {
        assert!(run_ok(&["pwd".to_string()], Some(Path::new("/"))).unwrap());
    }
}
