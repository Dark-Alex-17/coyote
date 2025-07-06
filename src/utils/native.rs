#[cfg(windows)]
pub mod runtime {
    use std::path::Path;

    pub fn bash_path() -> Option<String> {
        let bash_path = "C:\\Program Files\\Git\\bin\\bash.exe";
        if exist_path(bash_path) {
            return Some(bash_path.into());
        }
        let git_path = which("git")?;
        let git_parent_path = parent_path(&git_path)?;
        let bash_path = join_path(&parent_path(&git_parent_path)?, &["bin", "bash.exe"]);
        if exist_path(&bash_path) {
            return Some(bash_path);
        }
        let bash_path = join_path(&git_parent_path, &["bash.exe"]);
        if exist_path(&bash_path) {
            return Some(bash_path);
        }
        None
    }

    fn exist_path(path: &str) -> bool {
        Path::new(path).exists()
    }

    pub fn which(name: &str) -> Option<String> {
        which::which(name)
            .ok()
            .map(|path| path.to_string_lossy().into())
    }

    fn parent_path(path: &str) -> Option<String> {
        Path::new(path)
            .parent()
            .map(|path| path.to_string_lossy().into())
    }

    fn join_path(path: &str, parts: &[&str]) -> String {
        let mut path = Path::new(path).to_path_buf();
        for part in parts {
            path = path.join(part);
        }
        path.to_string_lossy().into()
    }
}
