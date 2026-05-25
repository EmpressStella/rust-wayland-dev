use crate::traits::CmdExecutor;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use tempfile::NamedTempFile;

pub struct LiveEnv;

impl CmdExecutor for LiveEnv {
    fn run_cmd(&self, cmd: &str, args: &[&str]) -> Result<(), std::io::Error> {
        let status = Command::new(cmd).args(args).status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "Command '{}' with args {:?} failed",
                cmd, args
            )));
        }
        Ok(())
    }
    fn run_cmd_ignore_err(&self, cmd: &str, args: &[&str]) -> Result<(), std::io::Error> {
        let _ = Command::new(cmd).args(args).status();
        Ok(())
    }
    fn run_cmd_in_dir(
        &self,
        dir: &std::path::Path,
        cmd: &str,
        args: &[&str],
    ) -> Result<(), std::io::Error> {
        let status = Command::new(cmd).args(args).current_dir(dir).status()?;
        if !status.success() {
            return Err(std::io::Error::other(format!(
                "Command '{}' with args {:?} failed",
                cmd, args
            )));
        }
        Ok(())
    }
    fn command_exists(&self, cmd: &str) -> bool {
        if cmd.is_empty() {
            return false;
        }
        let cmd_path = Path::new(cmd);
        if cmd_path.is_absolute() || cmd.contains('/') {
            return is_executable(cmd_path);
        }
        let Some(path_var) = std::env::var_os("PATH") else {
            return false;
        };
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(cmd);
            if is_executable(&candidate) {
                return true;
            }
        }
        false
    }
    fn read_file_to_string(&self, path: &std::path::Path) -> Result<String, std::io::Error> {
        std::fs::read_to_string(path)
    }
    fn get_env_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
    fn path_exists(&self, path: &std::path::Path) -> bool {
        path.exists()
    }
    fn path_is_dir(&self, path: &std::path::Path) -> bool {
        path.is_dir()
    }
    fn write_string_to_file(&self, path: &str, content: &str) -> Result<(), std::io::Error> {
        std::fs::write(path, content)
    }
    fn create_dir_all(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(path)
    }
    fn remove_dir_all(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        std::fs::remove_dir_all(path)
    }
    fn rename_path(
        &self,
        from: &std::path::Path,
        to: &std::path::Path,
    ) -> Result<(), std::io::Error> {
        std::fs::rename(from, to)
    }
    fn install_string_to_root_file(
        &self,
        dest_path: &std::path::Path,
        content: &str,
        mode: &str,
    ) -> Result<bool, std::io::Error> {
        if let Ok(existing_content) = self.read_file_to_string(dest_path)
            && existing_content == content
        {
            return Ok(false);
        }
        let mut temp_file = NamedTempFile::new()?;
        temp_file.write_all(content.as_bytes())?;
        let temp_path = temp_file.path();
        self.run_cmd(
            "sudo",
            &[
                "install",
                "-m",
                mode,
                "-o",
                "root",
                "-g",
                "root",
                temp_path.to_str().unwrap(),
                dest_path.to_str().unwrap(),
            ],
        )?;
        Ok(true)
    }
    fn create_root_dir_all(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        self.run_cmd("sudo", &["mkdir", "-p", path.to_str().unwrap()])?;
        self.run_cmd("sudo", &["chown", "root:root", path.to_str().unwrap()])?;
        Ok(())
    }
    fn list_dir_file_names(&self, path: &std::path::Path) -> Result<Vec<String>, std::io::Error> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }
    fn is_symlink(&self, path: &std::path::Path) -> bool {
        path.is_symlink()
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static PATH_LOCK: Mutex<()> = Mutex::new(());

    struct PathGuard {
        old: Option<OsString>,
    }

    impl PathGuard {
        fn set(path: &Path) -> Self {
            let old = env::var_os("PATH");
            env::set_var("PATH", path);
            Self { old }
        }
    }

    impl Drop for PathGuard {
        fn drop(&mut self) {
            if let Some(old) = self.old.take() {
                env::set_var("PATH", old);
            } else {
                env::remove_var("PATH");
            }
        }
    }

    fn make_executable(path: &Path) {
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn test_command_exists_searches_path() {
        let _lock = PATH_LOCK.lock().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = temp_dir.path().join("fakecmd");
        fs::write(&cmd_path, "#!/bin/sh\n").unwrap();
        make_executable(&cmd_path);

        let _guard = PathGuard::set(temp_dir.path());
        let env = LiveEnv;

        assert!(env.command_exists("fakecmd"));
        assert!(!env.command_exists("missingcmd"));
    }

    #[test]
    fn test_command_exists_absolute_path() {
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = temp_dir.path().join("notexec");
        fs::write(&cmd_path, "noop").unwrap();

        let env = LiveEnv;
        let cmd = PathBuf::from(&cmd_path);
        assert!(!env.command_exists(cmd.to_str().unwrap()));

        make_executable(&cmd_path);
        assert!(env.command_exists(cmd.to_str().unwrap()));
    }
}
