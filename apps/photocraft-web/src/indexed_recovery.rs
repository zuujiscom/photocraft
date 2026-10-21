//! Crash recovery in the browser (#6615): the desktop's autosave services (Preferences › File
//! Handling) with each document kept as a single-file .pcraft in IndexedDB instead of a recovery
//! folder, through craft-libs' `craft-web-recovery` (one store per tab session, held by a Web
//! Lock, so a second tab leaves a running tab's documents alone; a write counts only once
//! IndexedDB has committed it). Startup reads every entry before the app is built, so `recover`
//! answers at launch without waiting. Like the desktop's [`photocraft_format::RecoveryStore`], an
//! entry is only deleted once its document is saved or closed, never just because it was
//! recovered.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use craft_web_recovery::{Opened, Snapshot};
use photocraft_doc::Document;
use photocraft_ui_egui::{Recoverable, Services};

/// Open the store and take over what closed or crashed tabs left. Never fails: without IndexedDB
/// the app runs as before and autosaves report the problem.
pub async fn load() -> Opened {
    craft_web_recovery::open("photocraft").await
}

/// Install the crash-recovery services, as `apps/photocraft/src/services.rs` provides them on the
/// desktop. Returns the warnings worth showing.
pub fn install(services: &mut Services, opened: Opened, ctx: &egui::Context) -> Vec<String> {
    let Opened { store, recovered, warnings } = opened;
    let ctx = ctx.clone();
    store.set_waker(move || ctx.request_repaint());
    // Document id → its entry: `<session>-<id>`, or the recovered entry it adopted.
    let keys: Rc<RefCell<HashMap<u64, String>>> = Rc::default();
    let recovered = RefCell::new(recovered);

    let (s, k) = (store.clone(), keys.clone());
    services.autosave = Some(Box::new(move |doc: &Arc<Document>, revision: u64, path: Option<&str>| {
        // The browser has no worker for this: the snapshot is encoded on the page, once per
        // autosave interval and only for documents with changes.
        let bytes = photocraft_format::save_to_bytes(doc, &photocraft_format::SaveOptions::default()).map_err(|e| e.to_string())?;
        let key = k.borrow_mut().entry(doc.id.0).or_insert_with(|| s.key(doc.id.0)).clone();
        s.put(&key, revision, Snapshot { name: &doc.name, path, meta: None, bytes: &bytes })
    }));
    let (s, k) = (store.clone(), keys.clone());
    services.autosave_results = Some(Box::new(move || {
        let keys = k.borrow();
        s.take_written().into_iter().filter_map(|w| keys.iter().find(|(_, key)| **key == w.key).map(|(id, _)| (*id, w.revision, w.result))).collect()
    }));
    let k = keys.clone();
    services.discard_autosave = Some(Box::new(move |id: u64| {
        if let Some(key) = k.borrow_mut().remove(&id) {
            store.remove(&key);
        }
    }));
    services.recover = Some(Box::new(move || {
        std::mem::take(&mut *recovered.borrow_mut())
            .into_iter()
            .map(|r| Recoverable {
                key: r.key,
                name: r.name,
                path: r.path,
                load: Box::new(move || photocraft_format::load_from_bytes(&r.bytes).map_err(|e| e.to_string())),
            })
            .collect()
    }));
    services.adopt_autosave = Some(Box::new(move |id: u64, key: &str| {
        keys.borrow_mut().insert(id, key.to_string());
    }));
    warnings.iter().map(ToString::to_string).collect()
}
