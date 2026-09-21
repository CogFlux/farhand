//! The audit log: one JSON object per line, one file per day. It records
//! what was asked, where, and how it ended — never file contents and never
//! the text a guard rule matched.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

use crate::error::Result;

#[derive(Debug, Serialize)]
pub struct Record<'a> {
    pub ts: String,
    pub session: &'a str,
    pub host: &'a str,
    pub tool: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    pub outcome: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u128,
}

pub struct Audit {
    dir: PathBuf,
    session: String,
    host: String,
    file: Mutex<Option<(String, File)>>,
}

impl Audit {
    pub fn open(dir: PathBuf, host: &str) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let session = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            std::process::id()
        );
        Ok(Self {
            dir,
            session,
            host: host.to_string(),
            file: Mutex::new(None),
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Append one record. A failure to write the log is reported on stderr
    /// and otherwise ignored: losing an audit line must not block work,
    /// but it must not be silent either.
    pub fn record<'a>(&'a self, mut rec: Record<'a>) {
        rec.ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        rec.session = &self.session;
        rec.host = &self.host;
        let line = match serde_json::to_string(&rec) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("farhand: audit serialise failed: {e}");
                return;
            }
        };
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let needs_open = !matches!(&*guard, Some((d, _)) if *d == day);
        if needs_open {
            let path = self.dir.join(format!("{day}.jsonl"));
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => *guard = Some((day, f)),
                Err(e) => {
                    eprintln!("farhand: cannot open audit log {}: {e}", path.display());
                    return;
                }
            }
        }
        if let Some((_, f)) = &mut *guard {
            if let Err(e) = writeln!(f, "{line}") {
                eprintln!("farhand: audit write failed: {e}");
            }
        }
    }
}

impl<'a> Record<'a> {
    pub fn new(tool: &'a str) -> Self {
        Self {
            ts: String::new(),
            session: "",
            host: "",
            tool,
            command: None,
            path: None,
            target: None,
            bytes: None,
            outcome: "ok",
            exit_code: None,
            error: None,
            duration_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_line_per_record() {
        let tmp = tempfile::tempdir().unwrap();
        let audit = Audit::open(tmp.path().join("audit"), "devbox").unwrap();
        let mut r = Record::new("remote_shell");
        r.command = Some("ls");
        r.exit_code = Some(0);
        audit.record(r);
        let mut r = Record::new("upload");
        r.outcome = "denied";
        r.error = Some("rule: private-key-block".into());
        audit.record(r);
        let files: Vec<_> = std::fs::read_dir(tmp.path().join("audit"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1);
        let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["host"], "devbox");
        assert_eq!(v["command"], "ls");
        assert!(v["path"].is_null());
    }
}
