//! The secret guard. Nothing leaves this machine for the remote without
//! passing through here: command text, file contents written or edited,
//! and every file an upload would send.
//!
//! Two checks:
//! - **path**: the local file's name or any directory on its path marks it
//!   as credential material (`.env`, `*.pem`, `~/.ssh/...`, agent configs
//!   that embed API keys, ...);
//! - **content**: the bytes contain something shaped like a credential
//!   (private-key blocks, well-known token prefixes, a `key = "..."`
//!   assignment whose value is a literal, not a variable name, and not a
//!   low-entropy placeholder).
//!
//! The guard errs toward refusing. A false positive costs one retry with a
//! clear message; a false negative puts a credential on a machine the user
//! chose not to trust with it.

use std::path::{Component, Path};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use regex::bytes::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};

use crate::config::GuardConfig;
use crate::error::{Error, Result};

/// File names and globs that are credential material by convention.
/// Matched against the basename.
const DENIED_NAMES: &[&str] = &[
    ".env",
    ".env.*",
    "*.env",
    ".envrc",
    "*.pem",
    "*.key",
    "*.p12",
    "*.pfx",
    "*.jks",
    "*.keystore",
    "*.ppk",
    "*.ovpn",
    "*.kdbx",
    "*.gpg",
    "*.asc",
    "*.secret",
    "*.secrets",
    "secrets.*",
    "secret.*",
    "id_rsa*",
    "id_dsa*",
    "id_ecdsa*",
    "id_ed25519*",
    "authorized_keys",
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".yarnrc",
    ".git-credentials",
    ".htpasswd",
    ".vault-token",
    ".boto",
    ".s3cfg",
    ".pgpass",
    ".my.cnf",
    "credentials",
    "credentials.*",
    "*credentials*.json",
    "*service-account*.json",
    "*service_account*.json",
    "client_secret*.json",
    "*.tfstate",
    "*.tfstate.*",
    "*.tfvars",
    "terraform.rc",
    ".terraformrc",
    "kubeconfig",
    "opencode.json",
    "opencode.jsonc",
    ".farhand.toml",
    ".mcp.json",
    "mcp.json",
    "claude_desktop_config.json",
    ".claude.json",
    "auth.json",
    "hosts.yml",
    "wallet.dat",
];

/// Directory components that mark everything beneath them as off-limits.
const DENIED_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gcloud",
    ".gnupg",
    ".kube",
    ".docker",
    ".config",
    ".claude",
    ".opencode",
    ".codex",
    ".cursor",
    ".gemini",
    ".vscode-server",
    ".password-store",
    ".secrets",
    "secrets",
    ".1password",
    "Keychains",
    ".Trash",
];

/// Content shapes that are credentials. Each entry is `(rule name, regex)`;
/// the name is what the audit log and the refusal message carry, never the
/// matched text.
const DENIED_CONTENT: &[(&str, &str)] = &[
    (
        "private-key-block",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY( BLOCK)?-----",
    ),
    (
        "openai-or-anthropic-key",
        r"\bsk-(ant-|proj-|svcacct-)?[A-Za-z0-9_-]{20,}",
    ),
    (
        "github-token",
        r"\b(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})",
    ),
    ("aws-access-key", r"\b(AKIA|ASIA)[0-9A-Z]{16}\b"),
    ("google-api-key", r"\bAIza[0-9A-Za-z_-]{35}\b"),
    ("slack-token", r"\bxox[abprs]-[0-9A-Za-z-]{10,}"),
    ("stripe-key", r"\b[sr]k_(live|test)_[0-9A-Za-z]{20,}"),
    (
        "jwt",
        r"\beyJ[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{15,}\.[A-Za-z0-9_-]{10,}",
    ),
    ("huggingface-token", r"\bhf_[A-Za-z0-9]{30,}"),
    ("npm-token", r"\bnpm_[A-Za-z0-9]{36}\b"),
    ("gitlab-token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
    ("openrouter-key", r"\bsk-or-v1-[0-9a-f]{40,}"),
    (
        "bearer-header",
        r"(?i)\bauthorization\s*[:=]\s*['\x22]?\s*(bearer|basic|token)\s+[A-Za-z0-9._+/=-]{20,}",
    ),
    (
        "url-with-password",
        r"(?i)\b[a-z][a-z0-9+.-]*://[^\s/:@]+:[^\s/@]{6,}@[^\s/]+",
    ),
    ("credential-assignment", CREDENTIAL_ASSIGNMENT),
];

/// `key = value` with a credential-like key. A regex hit is only a
/// candidate: [`assigned_value_is_secret`] decides on the value, so
/// `token = args.token_from_command_line` is code, not a credential.
/// Group 1 is the opening quote of the value, if any; group 2 the value.
const CREDENTIAL_ASSIGNMENT: &str = r"(?i)\b(?:api[_-]?key|secret[_-]?key|access[_-]?key|private[_-]?key|auth[_-]?token|access[_-]?token|refresh[_-]?token|client[_-]?secret|password|passwd|token|secret)['\x22]?\s*[:=]\s*(['\x22]?)([A-Za-z0-9_./+=-]{24,})";

/// Below this many bits per byte a value is a placeholder (`xxxx…`,
/// `0000…`), not a credential. Random tokens of this length score above
/// 4; a human passphrase of 24+ characters still scores above 3.
const MIN_SECRET_ENTROPY: f64 = 3.0;

/// Names that may never be *created* locally by a download, on top of
/// [`DENIED_NAMES`] and [`DENIED_DIRS`]: files and directories an editor,
/// shell or agent executes or trusts when it opens the folder. A remote that
/// plants one of these inside the allowlist would otherwise get code run
/// locally, or repoint FarHand itself. Ordinary project files (package.json,
/// Makefile) are not listed: nothing runs them until the user does.
const LOCAL_TRUSTED_NAMES: &[&str] = &[
    ".farhand.toml",
    ".vscode",
    ".idea",
    ".git",
    ".husky",
    ".devcontainer",
    ".envrc",
    ".direnv",
    ".mise.toml",
    "mise.toml",
    ".pre-commit-config.yaml",
    "CLAUDE.md",
    "AGENTS.md",
    ".cursorrules",
    ".windsurfrules",
];

#[derive(Debug, Clone)]
pub struct Finding {
    pub rule: &'static str,
    pub detail: String,
}

impl Finding {
    fn deny(&self, what: &str) -> Error {
        Error::Denied(format!(
            "{what} looks like credential material (rule: {}, {}) and will not be sent to the \
             remote. If this is not a secret, rename it or rewrite the value; FarHand never \
             uploads credentials.",
            self.rule, self.detail
        ))
    }
}

/// How much of a file `ensure_upload_file` looks at per step.
const SCAN_CHUNK: usize = 1024 * 1024;
/// Bytes carried over between steps. Every `DENIED_CONTENT` match, and any
/// credential a `deny_content` pattern is meant to catch, is far shorter.
const SCAN_OVERLAP: usize = 64 * 1024;

fn any_case(pattern: &str) -> std::result::Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).case_insensitive(true).build()
}

/// Fill `buf` from `r` as far as the data goes; short only at EOF.
fn read_up_to(r: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

pub struct Guard {
    names: GlobSet,
    extra_paths: GlobSet,
    content_names: Vec<&'static str>,
    content: RegexSet,
    assignment: Regex,
    extra_content: Vec<Regex>,
}

impl Guard {
    pub fn new(cfg: &GuardConfig) -> Result<Self> {
        // Case-insensitive: on the default macOS and Windows volumes
        // `OPENCODE.JSONC` is the same file as `opencode.jsonc`.
        let mut names = GlobSetBuilder::new();
        for n in DENIED_NAMES {
            names.add(any_case(n).map_err(|e| Error::Config(e.to_string()))?);
        }
        let mut extra = GlobSetBuilder::new();
        for g in &cfg.deny_globs {
            extra.add(any_case(g).map_err(|e| Error::Config(format!("guard.deny_globs: {e}")))?);
        }
        // ASCII semantics for `\b` and `\s`: every credential shape above is
        // ASCII, and a Unicode word boundary makes the regex engine abandon
        // its DFA on each non-ASCII byte, which turns scanning a binary
        // file into a crawl (4 MB/s against 600 MB/s in practice).
        let content = RegexSetBuilder::new(DENIED_CONTENT.iter().map(|(_, r)| *r))
            .unicode(false)
            .build()
            .map_err(|e| Error::Config(e.to_string()))?;
        let assignment = RegexBuilder::new(CREDENTIAL_ASSIGNMENT)
            .unicode(false)
            .build()
            .map_err(|e| Error::Config(e.to_string()))?;
        let extra_content = cfg
            .deny_content
            .iter()
            .map(|r| Regex::new(r).map_err(|e| Error::Config(format!("guard.deny_content: {e}"))))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            names: names.build().map_err(|e| Error::Config(e.to_string()))?,
            extra_paths: extra.build().map_err(|e| Error::Config(e.to_string()))?,
            content_names: DENIED_CONTENT.iter().map(|(n, _)| *n).collect(),
            content,
            assignment,
            extra_content,
        })
    }

    /// Why a local path may not be uploaded, if it may not.
    pub fn check_path(&self, path: &Path) -> Option<Finding> {
        for comp in path.components() {
            if let Component::Normal(c) = comp {
                let s = c.to_string_lossy();
                if DENIED_DIRS.iter().any(|d| d.eq_ignore_ascii_case(&s)) {
                    return Some(Finding {
                        rule: "denied-directory",
                        detail: format!("path contains `{s}/`"),
                    });
                }
            }
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if self.names.is_match(name.as_ref()) {
            return Some(Finding {
                rule: "denied-filename",
                detail: format!("file name `{name}`"),
            });
        }
        if self.extra_paths.is_match(path) || self.extra_paths.is_match(name.as_ref()) {
            return Some(Finding {
                rule: "config-deny-glob",
                detail: format!("`{}` matches guard.deny_globs", path.display()),
            });
        }
        None
    }

    /// Why a download may not land at this local path (given relative to
    /// the allowlisted root), if it may not. Credential-shaped names are
    /// refused as on upload, plus anything an editor or agent would trust
    /// or execute when opening the folder.
    pub fn check_download_target(&self, rel: &Path) -> Option<Finding> {
        if let Some(f) = self.check_path(rel) {
            return Some(f);
        }
        for comp in rel.components() {
            if let Component::Normal(c) = comp {
                let s = c.to_string_lossy();
                if LOCAL_TRUSTED_NAMES
                    .iter()
                    .any(|d| d.eq_ignore_ascii_case(&s))
                {
                    return Some(Finding {
                        rule: "local-trusted-name",
                        detail: format!(
                            "`{s}` is something an editor or agent would execute or trust locally"
                        ),
                    });
                }
            }
        }
        None
    }

    /// Why these bytes may not be sent, if they may not.
    pub fn check_content(&self, bytes: &[u8]) -> Option<Finding> {
        for i in self.content.matches(bytes).iter() {
            let rule = self.content_names[i];
            if rule == "credential-assignment" && !self.has_secret_assignment(bytes) {
                continue;
            }
            return Some(Finding {
                rule,
                detail: "content matched".into(),
            });
        }
        for (i, re) in self.extra_content.iter().enumerate() {
            if re.is_match(bytes) {
                return Some(Finding {
                    rule: "config-deny-content",
                    detail: format!("guard.deny_content[{i}]"),
                });
            }
        }
        None
    }

    /// Whether any `CREDENTIAL_ASSIGNMENT` hit in `bytes` assigns a secret.
    fn has_secret_assignment(&self, bytes: &[u8]) -> bool {
        self.assignment
            .captures_iter(bytes)
            .any(|c| assigned_value_is_secret(&c[2], !c[1].is_empty()))
    }

    /// Refuse if `bytes` (described as `what` in the message) carry a secret.
    pub fn ensure_content(&self, what: &str, bytes: &[u8]) -> Result<()> {
        match self.check_content(bytes) {
            Some(f) => Err(f.deny(what)),
            None => Ok(()),
        }
    }

    /// Refuse if `path` or its `bytes` may not be uploaded.
    pub fn ensure_upload(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(f) = self.check_path(path) {
            return Err(f.deny(&format!("`{}`", path.display())));
        }
        self.ensure_content(&format!("the content of `{}`", path.display()), bytes)
    }

    /// `ensure_upload` for a file of any size: the content is scanned in
    /// windows that overlap by `SCAN_OVERLAP`, so a secret sitting on a
    /// window boundary is still seen and memory stays flat.
    pub fn ensure_upload_file(&self, path: &Path) -> Result<()> {
        self.ensure_upload_reader(path, &mut std::fs::File::open(path)?)
    }

    /// `ensure_upload_file` for a file already open as `file`.
    pub fn ensure_upload_reader(&self, path: &Path, file: &mut impl std::io::Read) -> Result<()> {
        if let Some(f) = self.check_path(path) {
            return Err(f.deny(&format!("`{}`", path.display())));
        }
        let mut window: Vec<u8> = Vec::with_capacity(SCAN_CHUNK + SCAN_OVERLAP);
        loop {
            let keep = window.len().saturating_sub(SCAN_OVERLAP);
            window.drain(..keep);
            let start = window.len();
            window.resize(start + SCAN_CHUNK, 0);
            let n = read_up_to(file, &mut window[start..])?;
            window.truncate(start + n);
            if n == 0 {
                return Ok(());
            }
            self.ensure_content(&format!("the content of `{}`", path.display()), &window)?;
        }
    }

    /// A scanner for content as it is sent: see [`StreamGuard`].
    pub fn stream(&self, path: &Path) -> StreamGuard<'_> {
        StreamGuard {
            guard: self,
            what: format!("the content of `{}`", path.display()),
            held: Vec::new(),
        }
    }

    /// Refuse if a download may not be written to `rel` (relative to the
    /// allowlisted root it lands in).
    pub fn ensure_download(&self, rel: &Path) -> Result<()> {
        match self.check_download_target(rel) {
            Some(f) => Err(Error::Denied(format!(
                "download refused: `{}` (rule: {}, {}). Downloads never create credential \
                 files or anything a local tool would execute; pick another name or fetch it \
                 yourself.",
                rel.display(),
                f.rule,
                f.detail
            ))),
            None => Ok(()),
        }
    }
}

/// Whether the value of a credential-like assignment is a credential.
/// A quoted value is a literal, so only placeholders pass. An unquoted one
/// may be code (`process.env.GITHUB_TOKEN`, `args.password_from_cli`), so a
/// value that reads as a dotted chain of identifiers passes too; an
/// unquoted `.env` or YAML secret is random, not made of words.
fn assigned_value_is_secret(value: &[u8], quoted: bool) -> bool {
    if entropy(value) < MIN_SECRET_ENTROPY {
        return false;
    }
    quoted || !reads_as_identifiers(value)
}

/// Shannon entropy of `bytes`, in bits per byte.
fn entropy(bytes: &[u8]) -> f64 {
    let mut counts = [0usize; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Whether `value` splits on `.` and `_` into words (`snake`, `SHOUT`,
/// `camelCase`, `PascalCase`, up to three trailing digits as in `sha256`),
/// with at least two words in all. One long unbroken run of letters is not
/// an identifier anyone writes; it is more likely a random password.
fn reads_as_identifiers(value: &[u8]) -> bool {
    let mut words = 0;
    for seg in value.split(|&b| b == b'.' || b == b'_') {
        if seg.is_empty() {
            continue;
        }
        match word_count(seg) {
            Some(n) => words += n,
            None => return false,
        }
    }
    words >= 2
}

/// How many words one identifier segment holds, or `None` if it is not
/// made of words. Random mixed-case text fails on the first two capitals
/// in a row after its start, or on digits anywhere but the end.
fn word_count(seg: &[u8]) -> Option<usize> {
    let digits = seg.iter().rev().take_while(|b| b.is_ascii_digit()).count();
    let body = &seg[..seg.len() - digits];
    if digits > 3 || body.is_empty() || !body.iter().all(u8::is_ascii_alphabetic) {
        return None;
    }
    // A leading run of capitals and/or lowercase (`HTTPServer`, `get`,
    // `TOKEN`) is the first word; each later word is a capital followed
    // by lowercase.
    let mut i = body.iter().take_while(|b| b.is_ascii_uppercase()).count();
    i += body[i..]
        .iter()
        .take_while(|b| b.is_ascii_lowercase())
        .count();
    let mut words = 1;
    while i < body.len() {
        let lower = body[i + 1..]
            .iter()
            .take_while(|b| b.is_ascii_lowercase())
            .count();
        if lower == 0 {
            return None;
        }
        i += 1 + lower;
        words += 1;
    }
    Some(words)
}

/// Checks an upload in the form it actually leaves in, chunk by chunk, so
/// a file that changes after its first scan cannot send what that scan
/// never saw. The last `SCAN_OVERLAP` bytes are held back until the next
/// chunk has been scanned with them, so a secret spanning two chunks is
/// seen whole before any of it is released — the same windows
/// `ensure_upload_file` uses.
pub struct StreamGuard<'g> {
    guard: &'g Guard,
    what: String,
    held: Vec<u8>,
}

impl StreamGuard<'_> {
    /// Add `data`; returns the bytes that may now be sent.
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.held.extend_from_slice(data);
        self.guard.ensure_content(&self.what, &self.held)?;
        let release = self.held.len().saturating_sub(SCAN_OVERLAP);
        Ok(self.held.drain(..release).collect())
    }

    /// The bytes still held back at the end of the data; they were
    /// scanned by the last `feed`.
    pub fn finish(self) -> Vec<u8> {
        self.held
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn guard() -> Guard {
        Guard::new(&GuardConfig::default()).unwrap()
    }

    #[test]
    fn stream_releases_nothing_a_later_chunk_could_complete() {
        let g = guard();
        let path = Path::new("data.bin");
        let mut first = vec![b'x'; SCAN_CHUNK];
        first.extend_from_slice(b" AKIAABCDEFGH");
        let mut s = g.stream(path);
        let sent = s.feed(&first).unwrap();
        assert_eq!(sent.len(), first.len() - SCAN_OVERLAP);
        assert!(!sent.windows(4).any(|w| w == b"AKIA"));
        let err = s.feed(b"IJKLMNOP and more").unwrap_err().to_string();
        assert!(err.contains("aws-access-key"), "{err}");

        // Clean data comes out whole, in order.
        let mut s = g.stream(path);
        let mut out = s.feed(&first[..100]).unwrap();
        out.extend(s.feed(&vec![b'y'; SCAN_CHUNK]).unwrap());
        out.extend(s.finish());
        assert_eq!(out.len(), 100 + SCAN_CHUNK);
        assert_eq!(&out[..100], &first[..100]);
    }

    #[test]
    fn scans_large_files_across_window_boundaries() {
        let g = guard();
        let dir = std::env::temp_dir().join(format!("farhand-guard-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clean = dir.join("clean.bin");
        std::fs::write(&clean, vec![b'x'; SCAN_CHUNK * 2 + 17]).unwrap();
        assert!(g.ensure_upload_file(&clean).is_ok());
        // A key that straddles the first window boundary.
        let mut bytes = vec![b'x'; SCAN_CHUNK - 8];
        bytes.extend_from_slice(b" AKIAABCDEFGHIJKLMNOP ");
        bytes.extend(std::iter::repeat_n(b'x', SCAN_CHUNK));
        let dirty = dir.join("dirty.bin");
        std::fs::write(&dirty, &bytes).unwrap();
        let err = g.ensure_upload_file(&dirty).unwrap_err().to_string();
        assert!(err.contains("aws-access-key"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn names_match_in_any_case() {
        let g = Guard::new(&GuardConfig {
            deny_globs: vec!["*.corp".into()],
            ..Default::default()
        })
        .unwrap();
        // None of these exist: the check is on the name a download would
        // create or an upload would send.
        for name in [
            "OPENCODE.JSONC",
            "OpenCode.json",
            ".ENV",
            ".Env.Local",
            "ID_RSA",
            "Server.PEM",
            ".FARHAND.TOML",
            ".MCP.json",
            "Notes.CORP",
        ] {
            let rel = PathBuf::from("dl").join(name);
            assert!(g.check_path(&rel).is_some(), "{name}");
            assert!(g.check_download_target(&rel).is_some(), "{name}");
        }
        assert!(g.check_path(Path::new("dl/README.md")).is_none());
    }

    #[test]
    fn download_targets() {
        let g = guard();
        for p in [
            ".claude/settings.json",
            "proj/.vscode/tasks.json",
            ".farhand.toml",
            "x/.git/hooks/pre-commit",
            "id_rsa",
            "sub/CLAUDE.md",
        ] {
            assert!(g.check_download_target(Path::new(p)).is_some(), "{p}");
        }
        for p in ["logs/app.log", "package.json", "src/main.rs", "Makefile"] {
            assert!(g.check_download_target(Path::new(p)).is_none(), "{p}");
        }
    }

    #[test]
    fn denies_well_known_files() {
        let g = guard();
        for p in [
            "/x/.env",
            "/x/.env.local",
            "/x/prod.env",
            "/x/server.pem",
            "/x/id_rsa",
            "/x/id_ed25519.pub",
            "/home/me/.ssh/config",
            "/home/me/.config/opencode/opencode.jsonc",
            "/x/.mcp.json",
            "/x/credentials.json",
            "/x/gcp-service-account-1234.json",
            "/x/terraform.tfstate",
            "/x/.aws/credentials",
        ] {
            assert!(
                g.check_path(&PathBuf::from(p)).is_some(),
                "{p} should be denied"
            );
        }
    }

    #[test]
    fn allows_ordinary_files() {
        let g = guard();
        for p in [
            "/x/main.rs",
            "/x/README.md",
            "/x/env.example.md",
            "/x/src/keyboard.rs",
        ] {
            assert!(
                g.check_path(&PathBuf::from(p)).is_none(),
                "{p} should be allowed"
            );
        }
    }

    #[test]
    fn denies_secret_shapes() {
        let g = guard();
        let cases: &[(&str, &str)] = &[
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nabc",
                "private-key-block",
            ),
            (
                "key: sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123",
                "openai-or-anthropic-key",
            ),
            ("ghp_abcdefghijklmnopqrstuvwxyz0123456789", "github-token"),
            ("github_pat_11ABCDEFG0123456789abcdefghij", "github-token"),
            ("AKIAIOSFODNN7EXAMPLE", "aws-access-key"),
            ("xoxb-1234567890-abcdefghij", "slack-token"),
            (
                "Authorization: Bearer abcdefghijklmnopqrstuvwxyz",
                "bearer-header",
            ),
            (
                "postgres://user:hunter2hunter2@db.internal/app",
                "url-with-password",
            ),
            (
                "API_KEY = \"a1b2c3d4e5f6g7h8i9j0k1l2m3n4\"",
                "credential-assignment",
            ),
            (
                "\"apiKey\": \"a1b2c3d4e5f6g7h8i9j0k1l2m3n4\"",
                "credential-assignment",
            ),
            // Quoted literals are refused even when they read as words.
            (
                "password = \"correct_horse_battery_staple\"",
                "credential-assignment",
            ),
            (
                "client_secret: 'aBcDeFgHiJkLmNoPqRsTuVwXyZ'",
                "credential-assignment",
            ),
            // Unquoted `.env` / YAML values that are not identifiers.
            ("TOKEN=Zx9Qm2Lp7Rt4Vb8Nc1Kd6Hf3Jg5", "credential-assignment"),
            ("PASSWORD=kqzjwhxmvtrplbnfgsdcyeua", "credential-assignment"),
            ("secret: QWERTYUIOPASDFGHJKLZXCVB", "credential-assignment"),
            (
                "access_token: dGhpcyBpcyBub3QgYSByZWFsIHRva2Vu",
                "credential-assignment",
            ),
            // A harmless assignment earlier in the text does not hide a
            // real one after it.
            (
                "token = challenge_response_json_token\napi_key = \"a1b2c3d4e5f6g7h8i9j0k1l2m3n4\"",
                "credential-assignment",
            ),
        ];
        for (text, rule) in cases {
            let f = g
                .check_content(text.as_bytes())
                .unwrap_or_else(|| panic!("{text} not caught"));
            assert_eq!(f.rule, *rule, "{text}");
        }
    }

    #[test]
    fn allows_ordinary_code() {
        let g = guard();
        for text in [
            "fn main() { println!(\"hello\"); }",
            "let token = parse_token(input);",
            "password = \"changeme\"",
            "const API_KEY_HEADER: &str = \"x-api-key\";",
            "export OPENAI_API_KEY=$(cat ~/.secrets/openai)",
            "curl -H 'Authorization: Bearer $TOKEN' https://x",
            "git clone https://github.com/CogFlux/farhand.git",
            // Variable names and attribute chains, not values.
            "token = challenge_response_json_token",
            "password = args.password_from_command_line",
            "token: process.env.GITHUB_TOKEN,",
            "secret_key = settings.SECRET_KEY_BASE_FOR_PROD",
            "let token = getAccessTokenFromHeader(req);",
            "password = config.db2.password_hash_sha256",
            "access_token = self.HTTPServer.currentAccessToken",
            // Placeholders.
            "\"apiKey\": \"xxxxxxxxxxxxxxxxxxxxxxxxxxxx\"",
            "API_KEY=000000000000000000000000",
        ] {
            assert!(
                g.check_content(text.as_bytes()).is_none(),
                "{text} wrongly denied"
            );
        }
    }

    #[test]
    fn extra_rules_apply() {
        let cfg = GuardConfig {
            deny_globs: vec!["*.internal".into()],
            deny_content: vec!["ACME-[0-9]{6}".into()],
        };
        let g = Guard::new(&cfg).unwrap();
        assert_eq!(
            g.check_path(Path::new("/x/notes.internal")).unwrap().rule,
            "config-deny-glob"
        );
        assert_eq!(
            g.check_content(b"ref ACME-123456").unwrap().rule,
            "config-deny-content"
        );
    }
}
