// file: src/layout.rs
// version: 1.1.0
// guid: 38e9eb5d-1113-4a32-b8ef-45ade70b5c39
// last-edited: 2026-10-10

//! Agent-owned CockroachDB binary layout.
//!
//! ```text
//! <root>/bin/cockroach                -> ../versions/cockroach-v25.3.0 (swapped atomically by the agent)
//! <root>/versions/cockroach-v25.3.0                                    (real executable)
//! ```
//!
//! The CockroachDB unit runs `<root>/bin/cockroach` through a systemd
//! drop-in that overrides `ExecStart`. `/usr/local/bin/cockroach` stays a
//! separate root-owned copy for operators. Root must never execute anything
//! under `<root>`, because the `cockroach` user can replace it.
//!
//! Every write the agent makes lands under `<root>`, which the `cockroach`
//! user owns, so the service can keep `NoNewPrivileges` and
//! `ProtectSystem=strict`. The swap is a `rename(2)` of a symlink, which is
//! atomic: CockroachDB sees either the old binary or the new one, never a
//! partially written file. A running process keeps its executable inode, so
//! the swap happens while the node is still up and only `systemctl restart`
//! causes downtime.

use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use semver::Version;

const BIN_DIR: &str = "bin";
const VERSIONS_DIR: &str = "versions";
const LINK_NAME: &str = "cockroach";

/// Paths of the agent-owned binary layout rooted at `root`.
#[derive(Debug, Clone)]
pub struct BinaryLayout {
    root: PathBuf,
}

impl BinaryLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join(BIN_DIR)
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.root.join(VERSIONS_DIR)
    }

    /// The symlink the agent swaps. The system binary path points here.
    pub fn current_link(&self) -> PathBuf {
        self.bin_dir().join(LINK_NAME)
    }

    pub fn version_file_name(version: &Version) -> String {
        format!("cockroach-v{version}")
    }

    pub fn version_path(&self, version: &Version) -> PathBuf {
        self.versions_dir().join(Self::version_file_name(version))
    }

    /// Relative link target stored in `bin/cockroach`, so the layout still
    /// resolves if `root` is bind-mounted or moved.
    fn link_target(version: &Version) -> PathBuf {
        Path::new("..")
            .join(VERSIONS_DIR)
            .join(Self::version_file_name(version))
    }

    /// Returns the file name `bin/cockroach` currently points at, for example
    /// `cockroach-v25.3.0`.
    pub fn current_target_name(&self) -> io::Result<String> {
        let target = fs::read_link(self.current_link())?;
        target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} points at {} which has no file name",
                        self.current_link().display(),
                        target.display()
                    ),
                )
            })
    }

    /// Checks that `exec_path`, the executable the CockroachDB unit starts,
    /// is this layout's current link, and that the link resolves to a file
    /// under `versions/`.
    pub fn verify_wired(&self, exec_path: &Path) -> Result<(), String> {
        if exec_path != self.current_link() {
            return Err(format!(
                "the CockroachDB unit runs {}, expected {}; run install-rollout-agent.sh to add the ExecStart drop-in",
                exec_path.display(),
                self.current_link().display()
            ));
        }
        let name = self.current_target_name().map_err(|error| {
            format!(
                "{} is not a readable symlink: {error}",
                self.current_link().display()
            )
        })?;
        let resolved = self.versions_dir().join(&name);
        if !resolved.is_file() {
            return Err(format!(
                "{} points at {name}, which is missing from {}",
                self.current_link().display(),
                self.versions_dir().display()
            ));
        }
        Ok(())
    }

    /// Copies `source` into `versions/` under the name for `version`.
    ///
    /// Writes to a temporary name first and renames into place, so a crash
    /// never leaves a truncated binary under a real version name.
    pub fn stage(&self, source: &Path, version: &Version) -> io::Result<PathBuf> {
        fs::create_dir_all(self.versions_dir())?;
        let destination = self.version_path(version);
        let partial = self
            .versions_dir()
            .join(format!(".{}.partial", Self::version_file_name(version)));
        fs::copy(source, &partial)?;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
        fs::File::open(&partial)?.sync_all()?;
        fs::rename(&partial, &destination)?;
        Ok(destination)
    }

    /// Atomically points `bin/cockroach` at the staged `version`.
    ///
    /// Returns the file name it pointed at before, which `restore` accepts
    /// for rollback.
    pub fn activate(&self, version: &Version) -> io::Result<Option<String>> {
        let staged = self.version_path(version);
        if !staged.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} has not been staged", staged.display()),
            ));
        }
        self.point_at(&Self::link_target(version))
    }

    /// Points `bin/cockroach` back at a previously active file name.
    pub fn restore(&self, previous_name: &str) -> io::Result<()> {
        let target = Path::new("..").join(VERSIONS_DIR).join(previous_name);
        if !self.versions_dir().join(previous_name).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("rollback target {previous_name} is missing"),
            ));
        }
        self.point_at(&target).map(|_| ())
    }

    fn point_at(&self, target: &Path) -> io::Result<Option<String>> {
        fs::create_dir_all(self.bin_dir())?;
        let previous = match self.current_target_name() {
            Ok(name) => Some(name),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let temporary = self.bin_dir().join(format!(".{LINK_NAME}.next"));
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        symlink(target, &temporary)?;
        fs::rename(&temporary, self.current_link())?;
        fs::File::open(self.bin_dir())?.sync_all()?;
        Ok(previous)
    }

    /// Deletes staged versions other than the active one and `keep`.
    pub fn prune(&self, keep: Option<&str>) -> io::Result<Vec<String>> {
        let active = self.current_target_name()?;
        let mut removed = Vec::new();
        for entry in fs::read_dir(self.versions_dir())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == active || Some(name.as_str()) == keep {
                continue;
            }
            if !name.starts_with("cockroach-v") && !name.ends_with(".partial") {
                continue;
            }
            fs::remove_file(entry.path())?;
            removed.push(name);
        }
        removed.sort();
        Ok(removed)
    }
}

/// Proves `dir` is writable by the current user by creating and removing a
/// probe file. Permission bits alone are not enough: systemd's
/// `ProtectSystem=strict` makes paths read-only through a mount namespace
/// without touching their mode.
pub fn probe_writable(dir: &Path) -> Result<(), String> {
    let probe = dir.join(format!(".write-probe-{}", std::process::id()));
    fs::write(&probe, b"probe")
        .map_err(|error| format!("{} is not writable by this user: {error}", dir.display()))?;
    fs::remove_file(&probe)
        .map_err(|error| format!("could not remove {}: {error}", probe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).expect("literal should parse")
    }

    fn fake_binary(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).expect("write fake binary");
        path
    }

    #[test]
    fn stage_activate_and_restore_round_trip() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let layout = BinaryLayout::new(scratch.path().join("agent"));
        let old = fake_binary(scratch.path(), "old", "old");
        let new = fake_binary(scratch.path(), "new", "new");

        layout.stage(&old, &version("25.3.0")).expect("stage old");
        assert_eq!(
            layout.activate(&version("25.3.0")).expect("activate old"),
            None
        );
        layout.stage(&new, &version("25.4.1")).expect("stage new");

        let previous = layout
            .activate(&version("25.4.1"))
            .expect("activate new")
            .expect("previous target recorded");
        assert_eq!(previous, "cockroach-v25.3.0");
        assert_eq!(fs::read_to_string(layout.current_link()).unwrap(), "new");

        layout.restore(&previous).expect("restore");
        assert_eq!(fs::read_to_string(layout.current_link()).unwrap(), "old");
    }

    #[test]
    fn staged_binary_is_executable() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let layout = BinaryLayout::new(scratch.path().join("agent"));
        let source = fake_binary(scratch.path(), "bin", "x");
        let staged = layout.stage(&source, &version("25.4.1")).expect("stage");
        let mode = fs::metadata(staged).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn activate_refuses_unstaged_version() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let layout = BinaryLayout::new(scratch.path().join("agent"));
        assert!(layout.activate(&version("25.4.1")).is_err());
    }

    #[test]
    fn verify_wired_accepts_the_expected_chain_only() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let layout = BinaryLayout::new(scratch.path().join("agent"));
        let source = fake_binary(scratch.path(), "bin", "x");
        layout.stage(&source, &version("25.3.0")).unwrap();
        layout.activate(&version("25.3.0")).unwrap();

        assert!(
            layout
                .verify_wired(Path::new("/usr/local/bin/cockroach"))
                .is_err(),
            "unit running the system binary must fail"
        );
        layout
            .verify_wired(&layout.current_link())
            .expect("unit running the agent link passes");

        fs::remove_file(layout.version_path(&version("25.3.0"))).unwrap();
        assert!(
            layout.verify_wired(&layout.current_link()).is_err(),
            "dangling agent link must fail"
        );
    }

    #[test]
    fn prune_keeps_active_and_rollback_target() {
        let scratch = tempfile::tempdir().expect("tempdir");
        let layout = BinaryLayout::new(scratch.path().join("agent"));
        let source = fake_binary(scratch.path(), "bin", "x");
        for text in ["25.2.9", "25.3.0", "25.4.1"] {
            layout.stage(&source, &version(text)).unwrap();
        }
        layout.activate(&version("25.4.1")).unwrap();

        let removed = layout.prune(Some("cockroach-v25.3.0")).unwrap();
        assert_eq!(removed, vec!["cockroach-v25.2.9".to_string()]);
        assert!(layout.version_path(&version("25.3.0")).is_file());
        assert!(layout.version_path(&version("25.4.1")).is_file());
    }
}
