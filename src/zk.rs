//! Runs the zk CLI.

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

/// `zk graph --format=json` in `root`. zk re-indexes changed notes first, so the output is
/// always current. The zk LSP indexes on save too; a concurrent run can hit SQLite's
/// "database is locked", which is retried with a short backoff.
pub fn graph(bin: &str, root: &Path, extra: &[String]) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for attempt in 0..4u64 {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(150 * attempt));
        }
        let out = Command::new(bin)
            .args(["graph", "--format=json", "--no-input"])
            .args(extra)
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("zk graph: cannot run {bin:?}: {e}"))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        last = format!("zk graph: {}: {msg}", out.status);
        if !msg.to_lowercase().contains("locked") {
            break;
        }
    }
    Err(last)
}
