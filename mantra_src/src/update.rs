//! Reuse the verified installer, updating beside the executable the user actually launched.

use std::path::Path;
use std::process::{Command, Stdio};

const INSTALLER: &str = include_str!("../../install.sh");

fn installer_command(executable: &Path) -> Result<Command, String> {
    let directory = executable.parent().filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| "cannot determine the installation directory".to_string())?;
    let mut command = Command::new("sh");
    command.arg("-c").arg(INSTALLER).arg("mantra-update")
        .env("MANTRA_INSTALL_DIR", directory)
        .env("MANTRA_UPDATE", "1")
        .stdin(Stdio::null());
    Ok(command)
}

pub fn run(args: &[String]) -> Result<(), String> {
    if matches!(args, [arg] if arg == "--help" || arg == "-h") {
        println!("mantra update — install the latest release of mantra and bija\n\nUpdates the directory containing this executable. Settings and sessions are kept.\nMANTRA_VERSION=vX.Y.Z selects a particular release.");
        return Ok(());
    }
    if !args.is_empty() {
        return Err("usage: mantra update (or mantra update --help)".into());
    }
    let executable = std::env::current_exe().map_err(|e| format!("locate mantra: {e}"))?;
    let status = installer_command(&executable)?.status().map_err(|e| format!("start updater: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("update failed ({status}); see the installer error above")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_uses_launched_binary_directory_without_shell_interpolation() {
        let command = installer_command(Path::new("/tmp/path with spaces/$(literal)/mantra")).unwrap();
        let env: Vec<_> = command.get_envs().collect();
        assert!(env.contains(&(std::ffi::OsStr::new("MANTRA_INSTALL_DIR"), Some(std::ffi::OsStr::new("/tmp/path with spaces/$(literal)")))));
        assert!(env.contains(&(std::ffi::OsStr::new("MANTRA_UPDATE"), Some(std::ffi::OsStr::new("1")))));
        assert_eq!(command.get_program(), "sh");
    }

    #[test]
    fn invalid_update_arguments_do_not_launch_installer() {
        assert!(run(&["--unknown".into()]).unwrap_err().contains("usage:"));
    }
}
