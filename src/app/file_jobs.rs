//! Bounded metadata refresh with path/read-generation fenced completions.
use super::*;

pub(super) enum FileMutation {
    CreateFile(PathBuf),
    CreateFolder(PathBuf),
    Rename {
        source: PathBuf,
        destination: PathBuf,
    },
    Delete(PathBuf),
}

impl FileMutation {
    fn path(&self) -> &std::path::Path {
        match self {
            Self::CreateFile(path) | Self::CreateFolder(path) | Self::Delete(path) => path,
            Self::Rename { destination, .. } => destination,
        }
    }

    fn execute(&self) -> std::io::Result<&'static str> {
        match self {
            Self::CreateFile(path) => {
                // Atomic create, including dangling symlinks: never truncate an
                // entry that appeared after the user opened the prompt.
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                Ok("created")
            }
            Self::CreateFolder(path) => {
                std::fs::create_dir(path)?;
                Ok("created")
            }
            Self::Rename {
                source,
                destination,
            } => {
                std::fs::symlink_metadata(source)?;
                std::fs::rename(source, destination)?;
                Ok("renamed")
            }
            Self::Delete(path) => {
                // Remove the selected link itself, not a linked directory tree.
                let metadata = std::fs::symlink_metadata(path)?;
                #[cfg(windows)]
                {
                    use std::os::windows::fs::FileTypeExt;
                    if metadata.file_type().is_symlink_dir() {
                        std::fs::remove_dir(path)?;
                        return Ok("deleted");
                    }
                }
                if metadata.is_dir() {
                    std::fs::remove_dir_all(path)?;
                } else {
                    std::fs::remove_file(path)?;
                }
                Ok("deleted")
            }
        }
    }
}

impl App {
    pub(super) fn schedule_file_mutation(
        &mut self,
        operation: FileMutation,
    ) -> Result<(), &'static str> {
        if self.file_mutation_inflight {
            return Err("a file operation is already pending");
        }
        let root = self.file_tree.root().to_path_buf();
        self.io_jobs.submit(self.app_tx.clone(), move || {
            let result = operation.execute().map_err(|e| e.to_string());
            Box::new(move |app| {
                app.file_mutation_inflight = false;
                match result {
                    Ok(message) => {
                        // The tree can retain its old root after a workspace
                        // switch or failed replacement-shell startup. Refresh
                        // only while its owning workspace is still active.
                        if app.file_tree.root() == root
                            && app
                                .workspaces
                                .get(app.active_ws)
                                .is_some_and(|ws| crate::platform::same_path(&ws.cwd, &root))
                        {
                            app.after_fs_change(operation.path());
                        }
                        app.show_toast(message);
                    }
                    Err(error) => app.show_toast(format!("file operation failed: {error}")),
                }
                true
            })
        })?;
        self.file_mutation_inflight = true;
        self.show_toast("file operation pending");
        Ok(())
    }

    pub(super) fn schedule_file_metadata(&mut self) {
        if self.file_metadata_inflight || self.views.is_empty() {
            return;
        }
        // Stable order gives every view service without an unbounded job input.
        let mut ids: Vec<_> = self.views.keys().copied().collect();
        ids.sort_by_key(|id| id.0);
        let start = self.file_metadata_cursor % ids.len();
        let count = ids.len().min(128);
        let inputs: Vec<_> = ids
            .iter()
            .cycle()
            .skip(start)
            .take(count)
            .filter_map(|id| match self.views.get(id)? {
                ViewKind::File(v) => Some((*id, v.path.clone(), v.read_token, v.mtime, false)),
                ViewKind::Preview(v) => Some((*id, v.path.clone(), v.read_token, v.mtime, true)),
                ViewKind::Diff(_) => None,
            })
            .collect();
        if inputs.is_empty() {
            self.file_metadata_cursor = start + count;
            return;
        }
        if self
            .io_jobs
            .submit(self.app_tx.clone(), move || {
                let changed: Vec<_> = inputs
                    .into_iter()
                    .filter_map(|(id, path, token, previous, preview)| {
                        let disk = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                        (disk.is_some() && disk != previous)
                            .then_some((id, path, token, previous, disk, preview))
                    })
                    .collect();
                Box::new(move |app| {
                    app.file_metadata_inflight = false;
                    let mut refreshed = false;
                    for (id, path, token, previous, disk, preview) in changed {
                        let valid = match app.views.get_mut(&id) {
                            Some(ViewKind::File(v))
                                if !preview
                                    && v.path == path
                                    && v.read_token == token
                                    && v.mtime == previous =>
                            {
                                v.mtime = disk;
                                true
                            }
                            Some(ViewKind::Preview(v))
                                if preview
                                    && v.path == path
                                    && v.read_token == token
                                    && v.mtime == previous =>
                            {
                                v.mtime = disk;
                                true
                            }
                            _ => false,
                        };
                        if valid {
                            refreshed = true;
                            if preview {
                                app.schedule_preview_read(id, path);
                            } else {
                                app.schedule_file_read(id, path);
                            }
                        }
                    }
                    refreshed
                })
            })
            .is_ok()
        {
            self.file_metadata_cursor = start + count;
            self.file_metadata_inflight = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_completion_skips_closed_or_replaced_workspace() {
        let _env = crate::persist::test_env("file-mutation-workspace");
        let root = crate::persist::config_dir();
        std::fs::create_dir_all(&root).unwrap();
        for scenario in ["closed", "workspace-switched", "tree-replaced"] {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut app = App::new(80, 24, tx).unwrap();
            app.workspaces[app.active_ws].cwd = root.clone();
            app.file_tree.set_root(root.clone());
            let path = root.join(scenario);
            app.schedule_file_mutation(FileMutation::CreateFile(path.clone()))
                .unwrap();
            let completion = loop {
                let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let AppEvent::IoCompleted(completion) = event {
                    break completion;
                }
            };
            match scenario {
                "closed" => app.workspaces.clear(),
                "workspace-switched" => {
                    app.workspaces[app.active_ws].cwd = root.join("other");
                }
                "tree-replaced" => app.file_tree.set_root(root.join("other")),
                _ => unreachable!(),
            }
            let last_git_status_at = app.last_git_status_at;
            assert!(app.handle_event(AppEvent::IoCompleted(completion)));
            assert!(path.exists(), "accepted mutation must still complete");
            assert!(!app.file_mutation_inflight);
            assert_eq!(app.last_git_status_at, last_git_status_at);
            assert_eq!(
                app.toast.as_ref().map(|(message, _)| message.as_str()),
                Some("created")
            );
            app.drain_io_jobs();
        }
    }

    #[test]
    fn create_never_truncates_existing_content() {
        let _env = crate::persist::test_env("file-create-collision");
        let path = crate::persist::config_dir().join("keep.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"keep this").unwrap();
        assert_eq!(
            FileMutation::CreateFile(path.clone())
                .execute()
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(path).unwrap(), b"keep this");
    }

    #[cfg(unix)]
    #[test]
    fn delete_directory_link_preserves_target() {
        let _env = crate::persist::test_env("file-delete-symlink");
        let root = crate::persist::config_dir();
        let target = root.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep"), b"keep").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        FileMutation::Delete(link.clone()).execute().unwrap();
        assert!(std::fs::symlink_metadata(link).is_err());
        assert!(target.join("keep").exists());
    }

    #[test]
    fn metadata_completion_rejects_replaced_read_and_closed_view() {
        let _env = crate::persist::test_env("metadata-generation");
        let path = crate::persist::config_dir().join("example.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut app = App::new(80, 24, tx).unwrap();
        let id = PaneId(10000);
        for close in [false, true] {
            let mut view = crate::files::FileView::new(path.clone());
            view.read_token = 10;
            app.views.insert(id, ViewKind::File(view));
            app.ensure_file_views();
            assert!(app.file_metadata_inflight);
            let completion = loop {
                let ev = rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let AppEvent::IoCompleted(completion) = ev {
                    break completion;
                }
            };
            if close {
                app.views.remove(&id);
            } else if let Some(ViewKind::File(v)) = app.views.get_mut(&id) {
                v.read_token = 11;
            }
            completion.apply(&mut app);
            assert!(!app.file_metadata_inflight);
            if !close {
                let Some(ViewKind::File(v)) = app.views.get(&id) else {
                    panic!("missing view")
                };
                assert_eq!(v.read_token, 11);
                assert_eq!(v.mtime, None);
            }
        }
        app.drain_io_jobs();
    }
}
