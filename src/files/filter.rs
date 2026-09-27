//! On-demand FILES fuzzy filtering with one worker and one pending query.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use super::VisibleRow;
use crate::event::AppEvent;
use crate::search::{FuzzyField, FuzzyQuery, PreparedText};

static NEXT_FILTER: AtomicU64 = AtomicU64::new(1);
const QUERY_CAP: usize = 256;

pub struct FileFilter {
    pub instance: u64,
    pub query: String,
    pub loading: bool,
    pub partial: bool,
    pub rows: Vec<VisibleRow>,
    pub generation: u64,
    pub saved_position: (usize, usize),
    pending: Arc<Mutex<Option<(u64, String)>>>,
    wake: mpsc::SyncSender<()>,
}

impl FileFilter {
    pub fn start(root: PathBuf, tx: mpsc::Sender<AppEvent>, position: (usize, usize)) -> Self {
        let instance = NEXT_FILTER.fetch_add(1, Ordering::Relaxed);
        let pending = Arc::new(Mutex::new(None::<(u64, String)>));
        let (wake, rx) = mpsc::sync_channel(1);
        let requests = Arc::clone(&pending);
        std::thread::spawn(move || {
            let catalog = crate::search::files::index_cached(&root);
            let prepared: Vec<_> = catalog
                .records
                .iter()
                .map(|record| PreparedText::new(&record.relative.to_string_lossy()))
                .collect();
            while rx.recv().is_ok() {
                let Some((generation, query)) = requests.lock().unwrap().take() else {
                    continue;
                };
                let query = FuzzyQuery::new(&query, false);
                let mut matches: Vec<_> = prepared
                    .iter()
                    .enumerate()
                    .filter_map(|(index, text)| {
                        query
                            .score(&[FuzzyField { text, weight: 1 }])
                            .map(|score| (index, score.value))
                    })
                    .collect();
                matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let partial = catalog.partial
                    || catalog.truncated
                    || matches.len() > crate::search::RESULT_CAP;
                let rows = matches
                    .into_iter()
                    .take(crate::search::RESULT_CAP)
                    .map(|(index, _)| {
                        let record = &catalog.records[index];
                        VisibleRow {
                            path: record.path.clone(),
                            name: record.relative.to_string_lossy().into_owned(),
                            depth: 0,
                            is_dir: false,
                            expanded: false,
                            loading: false,
                        }
                    })
                    .collect();
                if tx
                    .send(AppEvent::FileFilterResults {
                        instance,
                        generation,
                        rows,
                        partial,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut filter = Self {
            instance,
            query: String::new(),
            loading: true,
            partial: false,
            rows: Vec::new(),
            generation: 0,
            saved_position: position,
            pending,
            wake,
        };
        filter.recompute();
        filter
    }

    pub fn append(&mut self, text: &str) {
        let remaining = QUERY_CAP.saturating_sub(self.query.chars().count());
        self.query
            .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
        self.recompute();
    }

    pub fn recompute(&mut self) {
        self.generation += 1;
        self.loading = true;
        self.rows.clear();
        *self.pending.lock().unwrap() = Some((self.generation, self.query.clone()));
        let _ = self.wake.try_send(());
    }
}
