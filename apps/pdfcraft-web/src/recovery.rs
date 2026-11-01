//! Crash recovery in the browser: the app's autosave (`pdfcraft_ui_egui::RecoveryStore`) keeps its
//! copies in IndexedDB through craft-libs' `craft-web-recovery` (one store per tab session, held
//! by a Web Lock so a second tab leaves a running tab's copies alone, a write counted only once
//! IndexedDB has committed it). A copy is a document's working PDF, so it can be large: a full
//! disk or quota error is reported to the user once, and the copy is tried again at the next
//! autosave. Startup reads the copies of closed or crashed tabs before the app is built, so the
//! usual "Recover unsaved documents?" dialog offers them at launch.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use craft_web_recovery::{Opened, Recovered, Snapshot, Store};
use pdfcraft_ui_egui::{RecoveryMeta, WebSink};

pub async fn open() -> Opened {
    craft_web_recovery::open("pdfcraft").await
}

pub struct Sink {
    store: Store,
    /// Copies IndexedDB refused (key -> latest), written again at the next `put`.
    failed: RefCell<HashMap<String, (RecoveryMeta, Rc<Vec<u8>>)>>,
    /// Revision counter per key and the one each key last queued.
    revisions: RefCell<HashMap<String, u64>>,
    queued: RefCell<HashMap<(String, u64), (RecoveryMeta, Rc<Vec<u8>>)>>,
    message: RefCell<Option<String>>,
}

impl Sink {
    /// The sink and the copies earlier sessions left (newest first).
    pub fn new(opened: Opened) -> (Rc<Self>, Vec<(RecoveryMeta, Vec<u8>)>, Vec<String>) {
        let Opened { store, recovered, warnings } = opened;
        let found = recovered.into_iter().map(meta_of).collect();
        let sink = Rc::new(Sink {
            store,
            failed: RefCell::default(),
            revisions: RefCell::default(),
            queued: RefCell::default(),
            message: RefCell::default(),
        });
        (sink, found, warnings.iter().map(ToString::to_string).collect())
    }

    pub fn set_waker(&self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        self.store.set_waker(move || ctx.request_repaint());
    }

    /// Fold finished writes into `failed` and `message`.
    fn settle(&self) {
        for w in self.store.take_written() {
            let Some(copy) = self.queued.borrow_mut().remove(&(w.key.clone(), w.revision)) else { continue };
            match w.result {
                Ok(()) => {
                    // Only a still-current failure is cleared by a newer success.
                    self.failed.borrow_mut().remove(&w.key);
                }
                Err(e) => {
                    self.failed.borrow_mut().insert(w.key, copy);
                    self.message.borrow_mut().get_or_insert(e);
                }
            }
        }
    }

    fn queue(&self, meta: &RecoveryMeta, bytes: &Rc<Vec<u8>>) -> Result<(), String> {
        let revision = {
            let mut revisions = self.revisions.borrow_mut();
            let r = revisions.entry(meta.key.clone()).or_insert(0);
            *r += 1;
            *r
        };
        let json = serde_json::to_string(meta).map_err(|e| e.to_string())?;
        self.store.put(&meta.key, revision, Snapshot { name: &meta.name, path: meta.path.as_deref(), meta: Some(&json), bytes })?;
        self.queued.borrow_mut().insert((meta.key.clone(), revision), (meta.clone(), bytes.clone()));
        Ok(())
    }
}

fn meta_of(r: Recovered) -> (RecoveryMeta, Vec<u8>) {
    let parsed = r.meta.as_deref().and_then(|j| serde_json::from_str::<RecoveryMeta>(j).ok());
    let meta = match parsed {
        Some(mut m) => {
            m.key = r.key;
            m
        }
        None => RecoveryMeta { key: r.key, name: r.name, path: r.path, saved_at: (r.saved_at / 1000.0) as u64, encrypted: false, instance: None },
    };
    (meta, r.bytes)
}

impl WebSink for Sink {
    fn key(&self, doc: u64) -> String {
        self.store.key(doc)
    }

    fn put(&self, meta: &RecoveryMeta, bytes: &[u8]) -> Result<(), String> {
        self.settle();
        // Copies refused earlier are written again first (the newest of a key wins below).
        let retry: Vec<_> = self.failed.borrow().values().filter(|(m, _)| m.key != meta.key).cloned().collect();
        for (m, b) in retry {
            let _ = self.queue(&m, &b);
        }
        self.failed.borrow_mut().remove(&meta.key);
        self.queue(meta, &Rc::new(bytes.to_vec()))
    }

    fn remove(&self, key: &str) {
        self.failed.borrow_mut().remove(key);
        self.store.remove(key);
    }

    fn failure(&self) -> Option<String> {
        self.settle();
        self.message.borrow_mut().take()
    }
}
