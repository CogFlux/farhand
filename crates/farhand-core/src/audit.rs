//! The audit log: one JSON object per line, one file per day. It records
//! what was asked, where, and how it ended — never file contents and never
//! the text a guard rule matched.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

use crate::error::{Error, Result};

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
    /// Why a record could not be written, once one could not.
    failure: Mutex<Option<String>>,
}

impl Audit {
    /// Open the log in `dir`, creating it and today's file. A log that
    /// cannot be written is an error here, at startup: the alternative is a
    /// session whose every record quietly goes nowhere.
    pub fn open(dir: PathBuf, host: &str) -> Result<Self> {
        let unwritable = |e: std::io::Error| {
            Error::Config(format!(
                "the audit log directory {} is not writable: {e}",
                dir.display()
            ))
        };
        std::fs::create_dir_all(&dir).map_err(unwritable)?;
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{day}.jsonl")))
            .map_err(unwritable)?;
        let session = format!(
            "{}-{}",
            chrono::Utc::now().format("%Y%m%dT%H%M%S"),
            std::process::id()
        );
        Ok(Self {
            dir,
            session,
            host: host.to_string(),
            file: Mutex::new(Some((day, file))),
            failure: Mutex::new(None),
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// Why the log stopped being written, if it has. A session that has
    /// lost a record refuses every later call (see the server): the log is
    /// the record of what the model did, and it does not go on unrecorded.
    pub fn failure(&self) -> Option<String> {
        lock(&self.failure).clone()
    }

    /// Append one record. A failure to write it is reported on stderr and
    /// remembered in [`failure`](Self::failure).
    pub fn record<'a>(&'a self, mut rec: Record<'a>) {
        if let Err(e) = self.write(&mut rec) {
            eprintln!("farhand: {e}; refusing every further call");
            lock(&self.failure).get_or_insert(e);
        }
    }

    fn write<'a>(&'a self, rec: &mut Record<'a>) -> std::result::Result<(), String> {
        rec.ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        rec.session = &self.session;
        rec.host = &self.host;
        let line =
            serde_json::to_string(&rec).map_err(|e| format!("audit serialise failed: {e}"))?;
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let mut guard = lock(&self.file);
        let needs_open = !matches!(&*guard, Some((d, _)) if *d == day);
        if needs_open {
            let path = self.dir.join(format!("{day}.jsonl"));
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| format!("cannot open audit log {}: {e}", path.display()))?;
            *guard = Some((day, f));
        }
        let Some((_, f)) = &mut *guard else {
            unreachable!("opened above")
        };
        writeln!(f, "{line}").map_err(|e| format!("audit write failed: {e}"))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
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

    #[test]
    fn a_lost_record_is_remembered() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("audit");
        let audit = Audit::open(dir.clone(), "devbox").unwrap();
        audit.record(Record::new("remote_info"));
        assert!(audit.failure().is_none());
        // The day's file can no longer be opened.
        *lock(&audit.file) = None;
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, "").unwrap();
        audit.record(Record::new("remote_info"));
        assert!(audit.failure().unwrap().contains("cannot open"));
    }

    #[test]
    fn unwritable_dir_fails_at_open() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let err = Audit::open(file.join("audit"), "devbox").err().unwrap();
        assert!(err.to_string().contains("not writable"), "{err}");
    }
}
