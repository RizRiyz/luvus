//! Identity-bound removal for a confirmed sidebar worktree deletion.
//!
//! Capture the directory at a private, unique path before invoking destructive
//! Git operations. A checkout subsequently created at the original path is
//! never a deletion target. All filesystem and Git work runs on the IO worker.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::platform::DirectoryIdentity;

pub(crate) fn remove_confirmed(
    repo: &Path,
    path: &Path,
    identity: DirectoryIdentity,
) -> Result<(), String> {
    CapturedWorktree::capture(repo, path, identity)?.remove()
}

struct CapturedWorktree {
    repo: PathBuf,
    original: PathBuf,
    parent: PathBuf,
    path: PathBuf,
    identity: DirectoryIdentity,
    registration_touched: bool,
}

impl CapturedWorktree {
    fn capture(repo: &Path, path: &Path, identity: DirectoryIdentity) -> Result<Self, String> {
        Self::capture_with(repo, path, identity, || {})
    }

    fn capture_with(
        repo: &Path,
        path: &Path,
        identity: DirectoryIdentity,
        before_capture: impl FnOnce(),
    ) -> Result<Self, String> {
        let original = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
        ensure_identity(&original, identity)?;
        let linked = super::local::worktrees(repo)?.iter().any(|worktree| {
            !worktree.is_main
                && std::fs::canonicalize(&worktree.path)
                    .is_ok_and(|path| crate::platform::same_path(&path, &original))
        });
        if !linked {
            return Err("worktree removal target is not a linked worktree".into());
        }
        let base = original
            .parent()
            .ok_or("worktree has no parent directory")?;
        let parent = base.join(format!(".{}", crate::ids::public_id("luvus-delete")));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&parent).map_err(|error| error.to_string())?;
        let mut captured = Self {
            repo: repo.to_path_buf(),
            original,
            path: parent.join("checkout"),
            parent,
            identity,
            registration_touched: false,
        };
        before_capture();
        // This rename is non-destructive. Verify the captured object, rather
        // than trusting a check of the mutable source path immediately before it.
        std::fs::rename(&captured.original, &captured.path)
            .map_err(|error| format!("could not stage worktree removal: {error}"))?;
        if let Err(error) = ensure_identity(&captured.path, identity) {
            return Err(captured.restore(error));
        }
        // Repair registration after the atomic move, without relying on Git's
        // separate worktree-move restrictions.
        captured.registration_touched = true;
        if let Err(error) = git(&captured.repo, "repair", &captured.path) {
            return Err(captured.restore(error));
        }
        Ok(captured)
    }

    fn remove(self) -> Result<(), String> {
        if let Err(error) = ensure_identity(&self.path, self.identity) {
            return Err(format!(
                "{error}; no removal attempted at {}",
                self.path.display()
            ));
        }
        if let Err(error) = git(&self.repo, "remove", &self.path) {
            return Err(self.restore(error));
        }
        // Successful Git removal owns the entire captured checkout. Never
        // recursively delete the old path, or retry against a leftover directory.
        match std::fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(self.restore("Git left the staged checkout in place".into())),
            Err(error) => Err(format!("could not verify staged removal: {error}")),
        }
    }

    fn restore(&self, error: String) -> String {
        match crate::platform::rename_directory_noreplace(&self.path, &self.original) {
            Ok(()) if !self.registration_touched => error,
            Ok(()) => match git(&self.repo, "repair", &self.original) {
                Ok(()) => error,
                Err(repair) => format!(
                    "{error}; checkout restored at {}, but registration repair failed: {repair}",
                    self.original.display()
                ),
            },
            Err(restore) => format!(
                "{error}; checkout retained at {}: restoration failed: {restore}",
                self.path.display()
            ),
        }
    }
}

impl Drop for CapturedWorktree {
    fn drop(&mut self) {
        // Remove only an empty staging parent. Failed removal/rollback must
        // leave any retained checkout available for manual recovery.
        let _ = std::fs::remove_dir(&self.parent);
    }
}

fn ensure_identity(path: &Path, expected: DirectoryIdentity) -> Result<(), String> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && crate::platform::directory_identity(path) == Some(expected)
    {
        Ok(())
    } else {
        Err("worktree changed since deletion was confirmed".into())
    }
}

fn git(repo: &Path, operation: &str, path: &Path) -> Result<(), String> {
    let mut command = Command::new("git");
    command.current_dir(repo).args(["worktree", operation]);
    if operation == "remove" {
        command.arg("--force");
    }
    let output = crate::platform::no_window(command.arg("--").arg(path))
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        repo: PathBuf,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/issue420-fixtures")
                .join(crate::ids::public_id("test"));
            let repo = root.join("repo");
            let path = root.join("checkout");
            std::fs::create_dir_all(&repo).unwrap();
            std::fs::create_dir(root.join("hooks")).unwrap();
            let fixture = Self { root, repo, path };
            fixture.command(&["init", "-q"], None);
            fixture.command(
                &[
                    "-c",
                    "user.name=Luvus Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "initial",
                ],
                None,
            );
            fixture.command(
                &["worktree", "add", "-q", "-b", "feature"],
                Some(&fixture.path),
            );
            std::fs::write(fixture.path.join("untracked.txt"), "confirmed checkout").unwrap();
            fixture
        }

        fn command(&self, args: &[&str], path: Option<&Path>) -> String {
            let mut command = Command::new("git");
            command
                .arg("-c")
                .arg(format!(
                    "core.hooksPath={}",
                    self.root.join("hooks").display()
                ))
                .args(args)
                .current_dir(&self.repo);
            if let Some(path) = path {
                command.arg(path);
            }
            let output = crate::platform::no_window(&mut command).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn identity(&self) -> DirectoryIdentity {
            crate::platform::directory_identity(&self.path).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only this test's freshly-created, uniquely-named fixture root.
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn confirmed_removal_keeps_main_checkout_and_branch() {
        let fixture = Fixture::new();
        remove_confirmed(&fixture.repo, &fixture.path, fixture.identity()).unwrap();
        assert!(!fixture.path.exists());
        assert!(fixture.repo.join(".git").exists());
        fixture.command(&["show-ref", "--verify", "refs/heads/feature"], None);
        assert_eq!(
            super::super::local::worktrees(&fixture.repo).unwrap().len(),
            1
        );
        assert!(!std::fs::read_dir(&fixture.root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".luvus-delete_")
        }));
    }

    #[test]
    fn main_checkout_cannot_be_captured_for_removal() {
        let fixture = Fixture::new();
        let identity = crate::platform::directory_identity(&fixture.repo).unwrap();
        assert!(remove_confirmed(&fixture.repo, &fixture.repo, identity).is_err());
        assert!(fixture.repo.join(".git").exists());
        assert!(fixture.path.join("untracked.txt").exists());
    }

    #[test]
    fn confirmed_removal_supports_a_checkout_with_submodule_index_entries() {
        let fixture = Fixture::new();
        let commit = fixture.command(&["rev-parse", "HEAD"], None);
        let entry = format!("160000,{},nested", commit.trim());
        fixture.command(
            &[
                "-C",
                fixture.path.to_str().unwrap(),
                "update-index",
                "--add",
                "--cacheinfo",
                &entry,
            ],
            None,
        );
        std::fs::create_dir(fixture.path.join("nested")).unwrap();
        std::fs::write(fixture.path.join("nested/keep.txt"), "submodule checkout").unwrap();
        std::fs::write(
            fixture.path.join(".gitmodules"),
            "[submodule \"nested\"]\n\tpath = nested\n\turl = ./unused\n",
        )
        .unwrap();
        remove_confirmed(&fixture.repo, &fixture.path, fixture.identity()).unwrap();
        assert!(!fixture.path.exists());
        assert!(fixture.repo.join(".git").exists());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_symlink_substituted_during_capture_is_not_followed() {
        let fixture = Fixture::new();
        let moved = fixture.root.join("original");
        let identity = fixture.identity();
        let result = CapturedWorktree::capture_with(&fixture.repo, &fixture.path, identity, || {
            let output = crate::platform::no_window(
                Command::new("git")
                    .args(["worktree", "move"])
                    .arg(&fixture.path)
                    .arg(&moved)
                    .current_dir(&fixture.repo),
            )
            .output()
            .unwrap();
            assert!(output.status.success());
            std::os::unix::fs::symlink(&moved, &fixture.path).unwrap();
        });
        assert!(result.is_err());
        assert!(std::fs::symlink_metadata(&fixture.path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(moved.join("untracked.txt").exists());
        assert!(super::super::local::worktrees(&fixture.repo)
            .unwrap()
            .iter()
            .any(|worktree| crate::platform::same_path(&worktree.path, &moved)));
    }

    #[test]
    fn replacement_after_capture_survives_destructive_git_removal() {
        let fixture = Fixture::new();
        let captured =
            CapturedWorktree::capture(&fixture.repo, &fixture.path, fixture.identity()).unwrap();
        fixture.command(
            &["worktree", "add", "-q", "-b", "replacement"],
            Some(&fixture.path),
        );
        std::fs::write(fixture.path.join("keep.txt"), "replacement checkout").unwrap();
        let replacement_identity = fixture.identity();
        captured.remove().unwrap();
        assert_eq!(fixture.identity(), replacement_identity);
        assert_eq!(
            std::fs::read_to_string(fixture.path.join("keep.txt")).unwrap(),
            "replacement checkout"
        );
        assert_eq!(
            super::super::local::worktrees(&fixture.repo).unwrap().len(),
            2
        );
        fixture.command(&["show-ref", "--verify", "refs/heads/feature"], None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn replacement_in_the_last_check_capture_gap_is_restored_not_deleted() {
        let fixture = Fixture::new();
        let moved = fixture.root.join("original");
        let identity = fixture.identity();
        let result = CapturedWorktree::capture_with(&fixture.repo, &fixture.path, identity, || {
            let output = crate::platform::no_window(
                Command::new("git")
                    .args(["worktree", "move"])
                    .arg(&fixture.path)
                    .arg(&moved)
                    .current_dir(&fixture.repo),
            )
            .output()
            .unwrap();
            assert!(output.status.success());
            fixture.command(
                &["worktree", "add", "-q", "-b", "replacement"],
                Some(&fixture.path),
            );
            std::fs::write(fixture.path.join("keep.txt"), "replacement checkout").unwrap();
        });
        assert!(result.is_err());
        assert!(moved.join("untracked.txt").exists());
        assert_eq!(
            std::fs::read_to_string(fixture.path.join("keep.txt")).unwrap(),
            "replacement checkout"
        );
        assert_eq!(
            super::super::local::worktrees(&fixture.repo).unwrap().len(),
            3
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn failed_git_removal_restores_the_original_checkout() {
        let fixture = Fixture::new();
        let identity = fixture.identity();
        let captured = CapturedWorktree::capture(&fixture.repo, &fixture.path, identity).unwrap();
        fixture.command(&["worktree", "lock"], Some(&captured.path));
        assert!(captured.remove().is_err());
        assert_eq!(fixture.identity(), identity);
        assert!(fixture.path.join("untracked.txt").exists());
        assert_eq!(
            super::super::local::worktrees(&fixture.repo).unwrap().len(),
            2
        );
    }

    #[test]
    fn failed_git_removal_does_not_overwrite_a_replacement_checkout() {
        let fixture = Fixture::new();
        let captured =
            CapturedWorktree::capture(&fixture.repo, &fixture.path, fixture.identity()).unwrap();
        let retained = captured.path.clone();
        fixture.command(&["worktree", "lock"], Some(&captured.path));
        fixture.command(
            &["worktree", "add", "-q", "-b", "replacement"],
            Some(&fixture.path),
        );
        std::fs::write(fixture.path.join("keep.txt"), "replacement checkout").unwrap();
        let replacement_identity = fixture.identity();
        let error = captured.remove().unwrap_err();
        assert!(error.contains("checkout retained at"));
        assert_eq!(fixture.identity(), replacement_identity);
        assert!(retained.join("untracked.txt").exists());
        assert!(fixture.path.join("keep.txt").exists());
    }

    #[test]
    fn restoration_never_replaces_even_an_empty_destination_directory() {
        let fixture = Fixture::new();
        let source = fixture.root.join("restore-source");
        let destination = fixture.root.join("restore-destination");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(source.join("keep.txt"), "retained checkout").unwrap();
        let identity = crate::platform::directory_identity(&destination);
        assert!(crate::platform::rename_directory_noreplace(&source, &destination).is_err());
        assert_eq!(crate::platform::directory_identity(&destination), identity);
        assert!(source.join("keep.txt").exists());
        assert!(std::fs::read_dir(destination).unwrap().next().is_none());
    }
}
