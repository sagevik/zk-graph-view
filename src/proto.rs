//! The nvim protocol: newline-delimited JSON on stdin (nvim -> app) and stdout
//! (app -> nvim). stdout carries nothing else; logs go to stderr.
//!
//! ```text
//! nvim -> app  {"type":"show"} {"type":"hide"} {"type":"refresh"} {"type":"quit"}
//!              {"type":"current","id":"notebook/relative/path.md"}
//! app -> nvim  {"type":"ready","notes":N} {"type":"open","path":"/abs/path.md"}
//!              {"type":"error","message":"..."}
//! ```

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum In {
    Show,
    Hide,
    Refresh,
    Quit,
    Current {
        #[serde(default)]
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Out {
    Ready { notes: usize },
    Open { path: String },
    Error { message: String },
}

pub fn parse(line: &str) -> Result<In, String> {
    serde_json::from_str(line).map_err(|e| e.to_string())
}

static STDOUT: Mutex<()> = Mutex::new(());

/// Writes one message line. Safe to call from any thread.
pub fn send(msg: &Out) {
    let Ok(mut line) = serde_json::to_string(msg) else { return };
    line.push('\n');
    let _guard = STDOUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = std::io::stdout().lock();
    // A closed stdout means nvim is gone; stdin EOF handles the shutdown.
    let _ = out.write_all(line.as_bytes()).and_then(|_| out.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_incoming() {
        assert_eq!(parse(r#"{"type":"show"}"#), Ok(In::Show));
        assert_eq!(
            parse(r#"{"type":"current","id":"sub/b c.md"}"#),
            Ok(In::Current { id: "sub/b c.md".into() })
        );
        assert!(parse(r#"{"type":"bogus"}"#).is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn serializes_outgoing_like_the_go_version() {
        let s = |m: Out| serde_json::to_string(&m).unwrap();
        assert_eq!(s(Out::Ready { notes: 3 }), r#"{"type":"ready","notes":3}"#);
        assert_eq!(s(Out::Open { path: "/n/a.md".into() }), r#"{"type":"open","path":"/n/a.md"}"#);
        assert_eq!(s(Out::Error { message: "x".into() }), r#"{"type":"error","message":"x"}"#);
    }
}
