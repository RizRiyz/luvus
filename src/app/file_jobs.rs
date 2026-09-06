//! Bounded metadata refresh with path/read-generation fenced completions.
use super::*;

impl App {
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
