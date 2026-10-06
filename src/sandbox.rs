use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    /// bash commands execute inside a Docker container with the workspace
    /// mounted at /work (execution support lives in the bash tool).
    DockerIsolated,
}

impl SandboxMode {
    pub fn parse(s: &str) -> Self {
        match s {
            "read-only" => Self::ReadOnly,
            "danger-full-access" => Self::DangerFullAccess,
            "docker-isolated" => Self::DockerIsolated,
            _ => Self::WorkspaceWrite,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
            Self::DockerIsolated => "docker-isolated",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Jail {
    pub cwd: PathBuf,
    pub extra: Vec<PathBuf>,
    pub mode: SandboxMode,
    pub shell_allowlist: Vec<String>,
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p
    }
}

fn normalize_existing_or_parent(path: &Path) -> PathBuf {
    if let Ok(c) = path.canonicalize() {
        return strip_verbatim(c);
    }
    // The path may not exist yet, with one or more intermediate components
    // being symlinks pointing elsewhere. Walk up the ancestor chain to the
    // first ancestor that actually exists, canonicalize it, then re-append
    // the missing tail — the containment check then sees the real location
    // behind any symlink instead of the purely lexical path.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent() {
        if let Ok(c) = parent.canonicalize() {
            // `cur` itself is the deepest missing component under `parent`;
            // it belongs to the re-appended tail too.
            if let Some(name) = cur.file_name() {
                tail.push(name.to_os_string());
            }
            let mut out = strip_verbatim(c);
            for comp in tail.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        if let Some(name) = cur.file_name() {
            tail.push(name.to_os_string());
        }
        cur = parent.to_path_buf();
    }
    strip_verbatim(path.to_path_buf())
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let p = strip_verbatim(path.to_path_buf());
    let r = strip_verbatim(root.to_path_buf());
    p.starts_with(&r)
}

impl Jail {
    pub fn new(cwd: PathBuf, extra: Vec<PathBuf>, mode: SandboxMode, allow: Vec<String>) -> Self {
        Self {
            cwd,
            extra,
            mode,
            shell_allowlist: allow,
        }
    }

    pub fn allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.cwd.clone()];
        roots.extend(self.extra.iter().cloned());
        roots
    }

    /// Resolve a path against the jail. Only `DangerFullAccess` escapes
    /// containment; `DockerIsolated` behaves exactly like `WorkspaceWrite`
    /// because the file tools run on the host against the directory that is
    /// mounted into the container.
    pub fn resolve(&self, p: &str) -> Result<PathBuf> {
        let path = if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.cwd.join(p)
        };
        let resolved = normalize_existing_or_parent(&path);
        if self.mode == SandboxMode::DangerFullAccess {
            return Ok(resolved);
        }
        let ok = self.allowed_roots().iter().any(|root| {
            let r = normalize_existing_or_parent(root);
            path_is_within(&resolved, &r)
        });
        if !ok {
            bail!("path {} is outside the workspace jail", resolved.display());
        }
        Ok(resolved)
    }

    pub fn assert_write(&self, p: &str) -> Result<PathBuf> {
        if self.mode == SandboxMode::ReadOnly {
            bail!("sandbox is read-only; writes are blocked");
        }
        self.resolve(p)
    }

    /// Gate a shell command. `DockerIsolated` intentionally shares the
    /// `WorkspaceWrite` rules (chaining blocked, allowlist enforced): the
    /// command string runs inside the container, but the same policy decides
    /// what may run at all. Only `DangerFullAccess` bypasses the check.
    pub fn assert_shell(&self, command: &str) -> Result<()> {
        if self.mode == SandboxMode::DangerFullAccess {
            return Ok(());
        }
        if command
            .chars()
            .any(|c| matches!(c, ';' | '&' | '|' | '>' | '<' | '`' | '$' | '\n' | '\r'))
            || command.contains("$((")
            || command.contains("$(")
        {
            bail!("shell command chaining and substitution are blocked by the sandbox");
        }
        let first = command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .trim_matches('"')
            .trim_matches('\'');
        let base = Path::new(first)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(first)
            .to_ascii_lowercase();
        if self
            .shell_allowlist
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&base))
        {
            return Ok(());
        }
        bail!("shell command `{base}` is not on the sandbox allowlist");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn jail_blocks_outside_path() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["git".into()],
        );
        let outside = std::env::temp_dir().join("varynth-jail-should-fail.txt");
        if outside.starts_with(dir.path()) {
            return;
        }
        assert!(jail.resolve(outside.to_str().unwrap()).is_err());
        assert!(jail.resolve("inside.txt").is_ok());
    }

    #[test]
    fn allowlist_rejects_unknown_bin() {
        let dir = tempdir().unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec!["git".into()],
        );
        assert!(jail.assert_shell("format C:").is_err());
        assert!(jail.assert_shell("git status").is_ok());
        assert!(jail.assert_shell("git; format C:").is_err());
        assert!(jail.assert_shell("git && whoami").is_err());
    }

    #[test]
    fn symlink_escape_blocked_even_for_missing_paths() {
        let dir = tempdir().unwrap();
        let jail_root = dir.path();
        std::fs::create_dir_all(jail_root.join("real")).unwrap();
        let outside = tempdir().unwrap();
        if outside.path().starts_with(jail_root) {
            return;
        }
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_dir(outside.path(), jail_root.join("link"));
        #[cfg(not(windows))]
        let linked = std::os::unix::fs::symlink(outside.path(), jail_root.join("link"));
        if linked.is_err() {
            // Symlinks need OS privileges (e.g. Windows developer mode or
            // elevation). Directory junctions need neither, and canonicalize
            // resolves them the same way; fall back to one before skipping.
            #[cfg(windows)]
            {
                let made = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(jail_root.join("link"))
                    .arg(outside.path())
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !made {
                    // The environment cannot create links at all; skip rather
                    // than fail on environment restrictions.
                    return;
                }
            }
            #[cfg(not(windows))]
            return;
        }
        let jail = Jail::new(
            jail_root.to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        std::fs::write(outside.path().join("existing.txt"), b"x").unwrap();

        // Existing target behind the symlink resolves outside the jail.
        assert!(jail.resolve("link/existing.txt").is_err());
        // One missing level behind the symlink still resolves outside.
        assert!(jail.resolve("link/missing.txt").is_err());
        // Multi-level missing tail behind the symlink: the lexical fallback
        // used to pass containment here; the resolved location must not.
        assert!(jail.resolve("link/sub/dir/newfile.txt").is_err());
    }

    #[test]
    fn benign_missing_paths_still_resolve() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("real/deep")).unwrap();
        let jail = Jail::new(
            dir.path().to_path_buf(),
            vec![],
            SandboxMode::WorkspaceWrite,
            vec![],
        );
        assert!(jail.resolve("inside.txt").is_ok());
        assert!(jail.resolve("real/a.txt").is_ok());
        assert!(jail.resolve("real/deep/very/new.txt").is_ok());
    }

    #[test]
    fn sandbox_mode_roundtrip_including_docker_isolated() {
        for mode in [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::DangerFullAccess,
            SandboxMode::DockerIsolated,
        ] {
            assert_eq!(SandboxMode::parse(mode.as_str()), mode);
        }
        assert_eq!(
            SandboxMode::parse("docker-isolated"),
            SandboxMode::DockerIsolated
        );
        assert_eq!(SandboxMode::DockerIsolated.as_str(), "docker-isolated");
        // Unrecognized values keep falling back to workspace-write.
        assert_eq!(SandboxMode::parse("yolo"), SandboxMode::WorkspaceWrite);
    }

    #[test]
    fn docker_isolated_matches_workspace_write_rules() {
        let dir = tempdir().unwrap();
        let mk = |mode| Jail::new(dir.path().to_path_buf(), vec![], mode, vec!["git".into()]);
        let docker = mk(SandboxMode::DockerIsolated);
        let workspace = mk(SandboxMode::WorkspaceWrite);

        // File tools run on the host against the mounted dir, so resolution
        // and write gating must be identical to workspace-write.
        assert!(docker.resolve("inside.txt").is_ok());
        assert!(docker.assert_write("new.txt").is_ok());
        let outside = std::env::temp_dir().join("varynth-docker-jail-escape.txt");
        if !outside.starts_with(dir.path()) {
            assert_eq!(
                docker.resolve(outside.to_str().unwrap()).is_err(),
                workspace.resolve(outside.to_str().unwrap()).is_err()
            );
            assert!(docker.resolve(outside.to_str().unwrap()).is_err());
            assert!(docker.assert_write(outside.to_str().unwrap()).is_err());
        }

        // The shell allowlist still applies under docker-isolated.
        assert!(docker.assert_shell("git status").is_ok());
        assert!(docker.assert_shell("format C:").is_err());
        assert!(docker.assert_shell("git && whoami").is_err());
        assert!(docker.assert_shell("git $(whoami)").is_err());
    }

    #[test]
    fn normalize_reappends_multi_level_missing_tail() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("real")).unwrap();
        let p = dir.path().join("real/sub/deep/newfile.txt");
        // `p` and all its missing ancestors must resolve to the real location
        // of the first existing ancestor plus the missing tail — not be lost
        // or re-rooted by the ancestor walk.
        let out = normalize_existing_or_parent(&p);
        assert_eq!(
            out,
            dir.path()
                .join("real")
                .join("sub")
                .join("deep")
                .join("newfile.txt")
        );
    }
}
