/// File-system watch mode: auto-reingest .md files when they change.
use std::path::Path;
use std::sync::mpsc;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, Config, Event, EventKind};
use notify::event::{CreateKind, ModifyKind, RemoveKind};

use crate::db::{Db, ingest_file};
use crate::error::MksResult;

/// Watches `dir` recursively and re-ingests `.md` files as they are created,
/// modified, or removed. Blocks until the process is killed (Ctrl-C).
pub fn watch_dir(db: &Db, dir: &Path, collection: &str) -> MksResult<()> {
    let (tx, rx) = mpsc::channel::<notify::Result<Event>>();

    let mut watcher: RecommendedWatcher = RecommendedWatcher::new(
        move |res| { let _ = tx.send(res); },
        Config::default(),
    )
    .map_err(|e| crate::error::MksError::Other(e.to_string()))?;

    watcher
        .watch(dir, RecursiveMode::Recursive)
        .map_err(|e| crate::error::MksError::Other(e.to_string()))?;

    println!(
        "Watching {} (collection: {}). Press Ctrl-C to stop.",
        dir.display(),
        collection
    );

    for res in rx {
        match res {
            Ok(event) => handle_event(db, &event, collection),
            Err(e) => eprintln!("  watch error: {}", e),
        }
    }

    Ok(())
}

fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
}

fn handle_event(db: &Db, event: &Event, collection: &str) {
    let is_write = matches!(
        event.kind,
        EventKind::Create(CreateKind::File)
            | EventKind::Create(CreateKind::Any)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Any)
    );
    let is_remove = matches!(event.kind, EventKind::Remove(RemoveKind::File) | EventKind::Remove(RemoveKind::Any));

    for path in &event.paths {
        if !is_markdown(path) {
            continue;
        }
        if is_write && path.exists() {
            match ingest_file(db, path, collection, false) {
                Ok((id, skipped)) => {
                    if !skipped {
                        println!("  updated  {} ({})", path.display(), id);
                    }
                }
                Err(e) => eprintln!("  error    {}: {}", path.display(), e),
            }
        } else if is_remove {
            let path_str = path.display().to_string();
            if let Ok(Some(id)) = db.find_by_path(&path_str) {
                if db.delete_document(&id).is_ok() {
                    println!("  removed  {}", path.display());
                }
            }
        }
    }
}
