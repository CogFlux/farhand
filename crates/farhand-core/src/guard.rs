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
//!   assignment with a high-entropy value).
//!
//! The guard errs toward refusing. A false positive costs one retry with a
//! clear message; a false negative puts a credential on a machine the user
//! chose not to trust with it.

use std::path::{Component, Path};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use regex::bytes::{Regex, RegexSet, RegexSetBuilder};

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
    (
        "credential-assignment",
        r"(?i)\b(api[_-]?key|secret[_-]?key|access[_-]?key|private[_-]?key|auth[_-]?token|access[_-]?token|refresh[_-]?token|client[_-]?secret|password|passwd|token|secret)['\x22]?\s*[:=]\s*['\x22]?[A-Za-z0-9_./+=-]{24,}",
    ),
];

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
        let hits = self.content.matches(bytes);
        if let Some(i) = hits.iter().next() {
            return Some(Finding {
                rule: self.content_names[i],
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
        if let Some(f) = self.check_path(path) {
            return Err(f.deny(&format!("`{}`", path.display())));
        }
        let mut file = std::fs::File::open(path)?;
        let mut window: Vec<u8> = Vec::with_capacity(SCAN_CHUNK + SCAN_OVERLAP);
        loop {
            let keep = window.len().saturating_sub(SCAN_OVERLAP);
            window.drain(..keep);
            let start = window.len();
            window.resize(start + SCAN_CHUNK, 0);
            let n = read_up_to(&mut file, &mut window[start..])?;
            window.truncate(start + n);
            if n == 0 {
                return Ok(());
            }
            self.ensure_content(&format!("the content of `{}`", path.display()), &window)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn guard() -> Guard {
        Guard::new(&GuardConfig::default()).unwrap()
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
