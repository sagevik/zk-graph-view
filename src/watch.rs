//! Debounced change notifications for note files below a notebook root.
//!
//! inotify is not recursive, so each directory is watched on its own (new ones are added
//! as they appear). Hidden directories are never watched: `.zk/notebook.db` changes on
//! every index and would otherwise cause a reload loop, and `.git` can be huge.

use notify::event::{EventKind, ModifyKind};
use notify::{RecursiveMode, Watcher as _};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub struct Config {
    pub root: PathBuf,
    /// Lower-case extensions without the dot, e.g. "md".
    pub exts: HashSet<String>,
    pub debounce: Duration,
}

fn hidden(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| matches!(c, Component::Normal(s) if s.to_string_lossy().starts_with('.')))
    })
}

fn ext_of(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_lowercase())
}

/// Watches `dir` and every non-hidden directory below it.
fn add_tree(w: &mut notify::RecommendedWatcher, root: &Path, dir: &Path) {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if hidden(root, &d) {
            continue;
        }
        if let Err(e) = w.watch(&d, RecursiveMode::NonRecursive) {
            // e.g. fs.inotify.max_user_watches reached
            eprintln!("zk-graph: watch {}: {e}", d.display());
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            for entry in rd.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
    }
}

/// Starts watching; `on_change` runs (on the watcher thread) once per burst of changes.
pub fn spawn(cfg: Config, on_change: impl Fn() + Send + 'static) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher = notify::recommended_watcher(tx).map_err(|e| e.to_string())?;

    add_tree(&mut watcher, &cfg.root, &cfg.root);

    thread::Builder::new()
        .name("watch".into())
        .spawn(move || {
            let mut watcher = watcher; // keep alive for the life of the thread
            let mut deadline: Option<Instant> = None;
            loop {
                let ev = match deadline {
                    Some(d) => match rx.recv_timeout(d.saturating_duration_since(Instant::now())) {
                        Ok(ev) => ev,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            deadline = None;
                            on_change();
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    },
                    None => match rx.recv() {
                        Ok(ev) => ev,
                        Err(_) => return,
                    },
                };
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(e) => {
                        eprintln!("zk-graph: watch: {e}");
                        continue;
                    }
                };
                let mut relevant = false;
                for p in &ev.paths {
                    if hidden(&cfg.root, p) {
                        continue;
                    }
                    if matches!(ev.kind, EventKind::Create(_)) && p.is_dir() {
                        add_tree(&mut watcher, &cfg.root, p); // may already contain notes (mv, checkout)
                        relevant = true;
                        continue;
                    }
                    match ext_of(p) {
                        Some(e) if cfg.exts.contains(&e) => relevant = true,
                        // a removed/renamed directory full of notes: its entries send nothing
                        None if matches!(
                            ev.kind,
                            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
                        ) => relevant = true,
                        _ => {}
                    }
                }
                if relevant && !matches!(ev.kind, EventKind::Access(_)) {
                    deadline = Some(Instant::now() + cfg.debounce);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zkg-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(".zk")).unwrap();
        d
    }

    fn start(name: &str) -> (PathBuf, Arc<AtomicUsize>) {
        let root = tempdir(name);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        spawn(
            Config {
                root: root.clone(),
                exts: ["md".to_string()].into(),
                debounce: Duration::from_millis(80),
            },
            move || {
                c.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        thread::sleep(Duration::from_millis(50));
        (root, calls)
    }

    fn settle() {
        thread::sleep(Duration::from_millis(300));
    }

    #[test]
    fn burst_is_debounced() {
        let (root, calls) = start("burst");
        for _ in 0..5 {
            std::fs::write(root.join("a.md"), "x").unwrap();
            thread::sleep(Duration::from_millis(10));
        }
        settle();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ignores_hidden_dirs_and_other_extensions() {
        let (root, calls) = start("ignore");
        std::fs::write(root.join(".zk/notebook.db"), "x").unwrap();
        std::fs::write(root.join("image.png"), "x").unwrap();
        settle();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn watches_new_subdirectories() {
        let (root, calls) = start("subdir");
        std::fs::create_dir(root.join("sub")).unwrap();
        settle();
        calls.store(0, Ordering::SeqCst);
        std::fs::write(root.join("sub/b.md"), "x").unwrap();
        settle();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hidden_is_relative_to_root() {
        let root = Path::new("/home/u/.notes");
        assert!(!hidden(root, Path::new("/home/u/.notes/a.md")), "root itself may be hidden");
        assert!(hidden(root, Path::new("/home/u/.notes/.zk/notebook.db")));
        assert!(hidden(root, Path::new("/home/u/.notes/sub/.git/x")));
    }
}
