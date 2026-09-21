//! Configuration: where the remote is, what the local side may touch, and the
//! limits every operation runs under.
//!
//! Resolution order (first hit wins):
//! 1. an explicit path (`--config`),
//! 2. `$FARHAND_CONFIG`,
//! 3. `.farhand.toml` in the working directory,
//! 4. `~/.config/farhand/config.toml`.
//!
//! Whether FarHand is *active* in a directory — takes the agent's local
//! tools away and puts its remote ones in — depends on where the config
//! came from: a project file, an explicit path or the environment always
//! activate; the global file activates only when its `activation` is
//! `"always"` rather than the default `"project"`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// When the global config alone should make a session remote.
    #[serde(default)]
    pub activation: ActivationMode,
    pub remote: RemoteConfig,
    #[serde(default)]
    pub local: LocalConfig,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub audit: AuditConfig,
    #[serde(default)]
    pub guard: GuardConfig,
    #[serde(default)]
    pub approval: ApprovalConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    /// SSH destination: an alias from `~/.ssh/config`, or `user@host`.
    /// Authentication, jump hosts, ports and keys all come from the user's
    /// own OpenSSH configuration; FarHand never handles credentials itself.
    pub host: String,
    /// Directory relative paths resolve against and commands start in.
    /// Absolute (`/srv/app`, `C:\\work`), or `~` / `~/...` for the remote
    /// user's home.
    pub workdir: String,
    /// What the remote runs. Detected on connect from the SFTP root, so
    /// `auto` is fine; set it explicitly so the model is told before the
    /// first command which shell it is writing for.
    #[serde(default)]
    pub os: Os,
    /// Shell that runs every command, as argv. On a POSIX remote the
    /// command text is appended as one final argument, so the last element
    /// must accept a script (`-c`, `-lc`, ...). On Windows only the first
    /// element is used, as the PowerShell executable (`powershell.exe` by
    /// default; set `["pwsh"]` for PowerShell 7).
    #[serde(default = "default_shell")]
    pub shell: Vec<String>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Passed to ssh as `ServerAliveInterval`; keeps NAT tables warm.
    #[serde(default = "default_alive_interval")]
    pub server_alive_interval_secs: u64,
}

pub fn default_shell() -> Vec<String> {
    vec!["bash".into(), "-lc".into()]
}

/// The remote operating system family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    #[default]
    Auto,
    /// Linux, macOS, BSD: a POSIX shell, `timeout(1)`, `base64(1)`.
    Posix,
    /// Windows with OpenSSH Server: commands run in PowerShell.
    Windows,
}
fn default_connect_timeout() -> u64 {
    20
}
fn default_alive_interval() -> u64 {
    30
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalConfig {
    /// The only local directories the model may list, read, upload from or
    /// download into. Empty means the local filesystem is fully closed.
    #[serde(default)]
    pub allowed_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default = "default_command_timeout")]
    pub command_timeout_secs: u64,
    #[serde(default = "default_max_command_timeout")]
    pub max_command_timeout_secs: u64,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_max_read_bytes")]
    pub max_read_bytes: usize,
    #[serde(default = "default_max_transfer_bytes")]
    pub max_transfer_bytes: u64,
    #[serde(default = "default_max_transfer_files")]
    pub max_transfer_files: usize,
}

fn default_command_timeout() -> u64 {
    120
}
fn default_max_command_timeout() -> u64 {
    1800
}
fn default_max_output_bytes() -> usize {
    100 * 1024
}
fn default_max_read_bytes() -> usize {
    512 * 1024
}
fn default_max_transfer_bytes() -> u64 {
    200 * 1024 * 1024
}
fn default_max_transfer_files() -> usize {
    5000
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            command_timeout_secs: default_command_timeout(),
            max_command_timeout_secs: default_max_command_timeout(),
            max_output_bytes: default_max_output_bytes(),
            max_read_bytes: default_max_read_bytes(),
            max_transfer_bytes: default_max_transfer_bytes(),
            max_transfer_files: default_max_transfer_files(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Directory for the JSONL audit log. Defaults to
    /// `~/.local/share/farhand/audit`. There is deliberately no way to turn
    /// the log off: every remote action is recorded.
    pub log_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardConfig {
    /// Extra glob patterns (matched against the full local path and the
    /// basename) that may never be uploaded. Added to the built-in list.
    #[serde(default)]
    pub deny_globs: Vec<String>,
    /// Extra regular expressions that mark content as secret.
    #[serde(default)]
    pub deny_content: Vec<String>,
}

/// Whether the global config turns every session remote, or only the
/// projects that carry their own `.farhand.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationMode {
    /// Every session is remote.
    Always,
    /// Only directories with a `.farhand.toml` are remote; everywhere else
    /// the agent keeps its local tools and FarHand offers none. The
    /// default: installing FarHand must not take a machine's local tools
    /// away by surprise.
    #[default]
    Project,
}

/// Where a loaded config came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Explicit,
    Env,
    Project,
    Global,
}

/// A config together with where it was found and whether it activates
/// FarHand in the directory it was resolved for.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
    pub source: Source,
}

impl Loaded {
    pub fn active(&self) -> bool {
        match self.source {
            Source::Explicit | Source::Env | Source::Project => true,
            Source::Global => self.config.activation == ActivationMode::Always,
        }
    }
}

/// Whether an agent should stop and ask the user before a mutating tool
/// runs. FarHand itself has no UI; this is the one knob every agent entry
/// translates into that agent's own permission system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Prompt before every mutating tool; read-only tools run freely.
    /// The default.
    Ask,
    /// Run everything without prompting; the audit log is the only record.
    Auto,
    /// Prompt before every tool, read-only ones included.
    Strict,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalMode::Ask => "ask",
            ApprovalMode::Auto => "auto",
            ApprovalMode::Strict => "strict",
        }
    }
}

/// The tools that change something on the remote or in the outbox.
pub const MUTATING_TOOLS: &[&str] = &[
    "remote_shell",
    "remote_write",
    "remote_edit",
    "upload",
    "download",
];

/// The tools that only look.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "remote_read",
    "remote_ls",
    "remote_glob",
    "remote_grep",
    "local_ls",
    "local_read",
    "remote_info",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalConfig {
    /// Default for every mutating tool.
    #[serde(default = "default_approval")]
    pub mode: ApprovalMode,
    /// Per-tool overrides, keyed by tool name; `ask` or `auto` only.
    #[serde(default)]
    pub tools: BTreeMap<String, ApprovalMode>,
}

fn default_approval() -> ApprovalMode {
    ApprovalMode::Ask
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            mode: default_approval(),
            tools: BTreeMap::new(),
        }
    }
}

impl ApprovalConfig {
    /// For every tool, whether it prompts (`Ask`) or runs (`Auto`),
    /// overrides applied. `Strict` never appears in the result.
    pub fn effective(&self) -> BTreeMap<&'static str, ApprovalMode> {
        let mutating = match self.mode {
            ApprovalMode::Auto => ApprovalMode::Auto,
            ApprovalMode::Ask | ApprovalMode::Strict => ApprovalMode::Ask,
        };
        let read_only = match self.mode {
            ApprovalMode::Strict => ApprovalMode::Ask,
            ApprovalMode::Ask | ApprovalMode::Auto => ApprovalMode::Auto,
        };
        MUTATING_TOOLS
            .iter()
            .map(|t| (*t, mutating))
            .chain(READ_ONLY_TOOLS.iter().map(|t| (*t, read_only)))
            .map(|(t, m)| (t, self.tools.get(t).copied().unwrap_or(m)))
            .collect()
    }
}

impl Config {
    pub fn example() -> &'static str {
        EXAMPLE
    }

    /// Load following the resolution order in the module docs, with the
    /// project file looked for in the process's working directory.
    pub fn load(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
        let l = Self::load_in(None, explicit)?;
        Ok((l.config, l.path))
    }

    /// Load following the resolution order, with the project file looked
    /// for in `cwd` (default: the process's working directory).
    pub fn load_in(cwd: Option<&Path>, explicit: Option<&Path>) -> Result<Loaded> {
        let (path, source) = match explicit {
            Some(p) => (p.to_path_buf(), Source::Explicit),
            None => Self::locate(cwd)?,
        };
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        // Relative `allowed_dirs` entries are resolved against the file's own
        // directory, but only for a per-project file: the global config
        // belongs to no directory, so `"."` would mean nothing there.
        let base = match source {
            Source::Global => None,
            _ => path.parent().map(Path::to_path_buf),
        };
        let config = Self::parse_at(&text, base.as_deref())?;
        Ok(Loaded {
            config,
            path,
            source,
        })
    }

    /// Parse a config that belongs to no directory: `allowed_dirs` entries
    /// must be absolute or `~/...`.
    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_at(text, None)
    }

    /// Parse a config file that lives in `base`; relative `allowed_dirs`
    /// entries (`"."`, `"assets"`) are resolved against it.
    pub fn parse_at(text: &str, base: Option<&Path>) -> Result<Self> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        cfg.validate(base)?;
        Ok(cfg)
    }

    fn locate(cwd: Option<&Path>) -> Result<(PathBuf, Source)> {
        if let Some(p) = std::env::var_os("FARHAND_CONFIG") {
            return Ok((PathBuf::from(p), Source::Env));
        }
        let local = match cwd {
            Some(d) => d.join(".farhand.toml"),
            None => PathBuf::from(".farhand.toml"),
        };
        if local.is_file() {
            return Ok((local, Source::Project));
        }
        let p = Self::global_path();
        if p.is_file() {
            return Ok((p, Source::Global));
        }
        Err(Error::NoConfig)
    }

    pub fn global_path() -> PathBuf {
        xdg_dir("XDG_CONFIG_HOME", ".config")
            .join("farhand")
            .join("config.toml")
    }

    fn validate(&mut self, base: Option<&Path>) -> Result<()> {
        if self.remote.host.trim().is_empty() {
            return Err(Error::Config("remote.host is empty".into()));
        }
        if self.remote.host.starts_with('-') {
            return Err(Error::Config("remote.host may not start with '-'".into()));
        }
        let raw = self.remote.workdir.trim();
        let w = raw.trim_end_matches(['/', '\\']);
        let w = if w.is_empty() && raw.starts_with('/') {
            "/".to_string()
        } else if is_drive_root(raw) {
            // `C:\` keeps its separator; `C:` alone would be drive-relative.
            format!("{}\\", &raw[..2])
        } else {
            w.to_string()
        };
        let windows_abs = w.len() >= 3
            && w.as_bytes()[0].is_ascii_alphabetic()
            && w.as_bytes()[1] == b':'
            && (w.as_bytes()[2] == b'\\' || w.as_bytes()[2] == b'/');
        if !(w.starts_with('/')
            || w == "~"
            || w.starts_with("~/")
            || w.starts_with("~\\")
            || windows_abs)
        {
            return Err(Error::Config(
                "remote.workdir must be an absolute path (`/srv/app`, `C:\\work`) or start with `~` \
                 (the remote home)"
                    .into(),
            ));
        }
        self.remote.workdir = w;
        if self.remote.shell.is_empty() {
            return Err(Error::Config("remote.shell is empty".into()));
        }
        if self.limits.command_timeout_secs == 0 || self.limits.max_command_timeout_secs == 0 {
            return Err(Error::Config("timeouts must be positive".into()));
        }
        let mut dirs = Vec::with_capacity(self.local.allowed_dirs.len());
        let home = dirs::home_dir();
        for d in &self.local.allowed_dirs {
            let mut expanded = expand_home(d);
            if !expanded.is_absolute() {
                // A relative entry names the project folder itself or
                // something inside it; `..` would let a config that arrived
                // with a repository reach above its own directory.
                let Some(base) = base else {
                    return Err(Error::Config(format!(
                        "local.allowed_dirs entry `{}` is relative; only a project \
                         .farhand.toml may use relative entries (`\".\"` for its own folder)",
                        d.display()
                    )));
                };
                if expanded
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(Error::Config(format!(
                        "local.allowed_dirs entry `{}` may not contain `..`",
                        d.display()
                    )));
                }
                expanded = base.join(expanded);
            }
            // The allowlist is for a folder the user set aside, never the
            // whole machine or home: that would hand every non-credential
            // file to the model, and a project-level config could do it
            // without the user noticing.
            if expanded.parent().is_none() || home.as_deref() == Some(expanded.as_path()) {
                return Err(Error::Config(format!(
                    "local.allowed_dirs may not contain `/` or the home directory ({}); \
                     allow a specific folder instead",
                    d.display()
                )));
            }
            dirs.push(expanded);
        }
        self.local.allowed_dirs = dirs;
        if let Some(d) = &self.audit.log_dir {
            self.audit.log_dir = Some(expand_home(d));
        }
        // `remote_bash` was the tool's name before 0.2.1; keep old configs
        // working.
        if let Some(m) = self.approval.tools.remove("remote_bash") {
            self.approval
                .tools
                .entry("remote_shell".into())
                .or_insert(m);
        }
        for (t, m) in &self.approval.tools {
            if !MUTATING_TOOLS.contains(&t.as_str()) && !READ_ONLY_TOOLS.contains(&t.as_str()) {
                return Err(Error::Config(format!(
                    "approval.tools: `{t}` is not a tool (one of {}, {})",
                    MUTATING_TOOLS.join(", "),
                    READ_ONLY_TOOLS.join(", ")
                )));
            }
            if *m == ApprovalMode::Strict {
                return Err(Error::Config(format!(
                    "approval.tools.{t}: use `ask` or `auto` (`strict` is a global mode)"
                )));
            }
        }
        Ok(())
    }

    pub fn audit_dir(&self) -> PathBuf {
        self.audit.log_dir.clone().unwrap_or_else(|| {
            xdg_dir("XDG_DATA_HOME", ".local/share")
                .join("farhand")
                .join("audit")
        })
    }
}

/// `$VAR` if set, else `~/<fallback>`. The XDG layout is used on every
/// platform so the documented paths (`~/.config/farhand`,
/// `~/.local/share/farhand`) are the real ones on macOS too.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    if let Some(v) = std::env::var_os(var) {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(fallback)
}

/// `~` and `~/x` expand to the home directory; anything else is returned as-is.
/// `C:\`, `C:/` or `C:` — a Windows drive root, with or without separator.
fn is_drive_root(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() == 2 || b.len() == 3)
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'\\' || b[2] == b'/')
}

pub fn expand_home(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return dirs::home_dir().unwrap_or_else(|| p.to_path_buf());
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

const EXAMPLE: &str = r#"# FarHand configuration.
# As .farhand.toml in a project root, it makes that project remote.
# As ~/.config/farhand/config.toml it is optional: only needed for
# activation = "always" (or as the target of --config / FARHAND_CONFIG).

# "project" (default): only directories that carry a .farhand.toml are
#            remote; anywhere else the agent keeps its local tools and FarHand
#            stays out of the way.
# "always":  every agent session on this machine is remote, using the
#            [remote] section below.
# Only meaningful in this global file; a project's .farhand.toml is always
# active in its own directory.
# activation = "project"

[remote]
# An alias from ~/.ssh/config or user@host. Keys, ports, jump hosts and
# agent forwarding all come from your OpenSSH config.
host = "devbox"
# Directory on the remote where every command starts and relative paths
# resolve. Absolute, or `~/...` for the remote user's home.
workdir = "~/project"
# os = "auto"              # "posix" or "windows"; detected on connect, but set it so
#                          # the model knows which shell it is writing for
# shell = ["bash", "-lc"]  # Windows: ["powershell"] (default) or ["pwsh"]
# connect_timeout_secs = 20
# server_alive_interval_secs = 30

[local]
# The only local directories the model may list, read from, upload from or
# download into. Leave empty to close the local filesystem completely.
# In a project .farhand.toml, "." means this folder: drop files here to
# upload them, and downloads land here. Never allow your home directory.
allowed_dirs = ["."]

[limits]
# command_timeout_secs = 120
# max_command_timeout_secs = 1800
# max_output_bytes = 102400
# max_read_bytes = 524288
# max_transfer_bytes = 209715200
# max_transfer_files = 5000

[audit]
# log_dir = "~/.local/share/farhand/audit"

[guard]
# Extra globs that may never leave this machine (added to the built-in list).
# deny_globs = ["*.internal"]
# Extra regexes that mark content as secret.
# deny_content = ["ACME-[0-9]{12}"]

[approval]
# "ask":    the agent prompts you before remote_shell, remote_write,
#           remote_edit, upload and download; read-only tools run freely.
# "auto":   nothing prompts; the audit log is your record.
# "strict": every tool prompts, reads and searches included.
mode = "ask"
# Per-tool overrides (`ask` or `auto`), any tool name:
# tools = { remote_write = "auto", remote_edit = "auto", remote_read = "ask" }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        // `farhand init` writes the example as a project file; its `"."`
        // is that file's folder, and is meaningless in the global config.
        let cfg = Config::parse_at(Config::example(), Some(Path::new("/proj"))).unwrap();
        assert_eq!(cfg.remote.host, "devbox");
        assert_eq!(cfg.remote.shell, vec!["bash", "-lc"]);
        assert_eq!(cfg.local.allowed_dirs, vec![PathBuf::from("/proj")]);
        assert!(Config::parse(Config::example()).is_err());
    }

    #[test]
    fn rejects_home_and_root_as_allowed_dirs() {
        for d in ["/", "~"] {
            let err = Config::parse(&format!(
                "[remote]\nhost='h'\nworkdir='/w'\n[local]\nallowed_dirs=['{d}']"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("home directory"), "{d}: {err}");
        }
        assert!(
            Config::parse("[remote]\nhost='h'\nworkdir='/w'\n[local]\nallowed_dirs=['~/out']")
                .is_ok()
        );
    }

    #[test]
    fn relative_allowed_dirs_resolve_against_the_project_file() {
        let text = "[remote]\nhost='h'\nworkdir='/w'\n[local]\nallowed_dirs=['.', 'assets']";
        let err = Config::parse(text).unwrap_err();
        assert!(err.to_string().contains("relative"), "{err}");
        let cfg = Config::parse_at(text, Some(Path::new("/proj"))).unwrap();
        assert_eq!(
            cfg.local.allowed_dirs,
            vec![PathBuf::from("/proj"), PathBuf::from("/proj/assets")]
        );
        let err = Config::parse_at(
            "[remote]\nhost='h'\nworkdir='/w'\n[local]\nallowed_dirs=['../x']",
            Some(Path::new("/proj")),
        )
        .unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
    }

    #[test]
    fn remote_bash_is_an_alias_for_remote_shell() {
        let cfg = Config::parse(
            "[remote]\nhost='h'\nworkdir='/'\n[approval]\nmode='auto'\ntools={ remote_bash='ask' }",
        )
        .unwrap();
        assert_eq!(cfg.approval.effective()["remote_shell"], ApprovalMode::Ask);
    }

    #[test]
    fn rejects_relative_workdir() {
        let err = Config::parse("[remote]\nhost='h'\nworkdir='rel'").unwrap_err();
        assert!(err.to_string().contains("absolute"));
        let cfg = Config::parse("[remote]\nhost='h'\nworkdir='~/'").unwrap();
        assert_eq!(cfg.remote.workdir, "~");
        let cfg = Config::parse("[remote]\nhost='h'\nworkdir='/srv/app/'").unwrap();
        assert_eq!(cfg.remote.workdir, "/srv/app");
        let cfg = Config::parse("[remote]\nhost='h'\nworkdir='C:\\work\\'").unwrap();
        assert_eq!(cfg.remote.workdir, "C:\\work");
        let cfg = Config::parse("[remote]\nhost='h'\nworkdir='D:/'").unwrap();
        assert_eq!(cfg.remote.workdir, "D:\\");
        let cfg = Config::parse("[remote]\nhost='h'\nworkdir='C:'").unwrap();
        assert_eq!(cfg.remote.workdir, "C:\\");
        assert!(Config::parse("[remote]\nhost='h'\nworkdir='work'").is_err());
        let cfg = Config::parse("[remote]\nhost='h'\nworkdir='/'\nos='windows'").unwrap();
        assert_eq!(cfg.remote.os, Os::Windows);
    }

    #[test]
    fn approval_overrides_apply() {
        let cfg = Config::parse(
            "[remote]\nhost='h'\nworkdir='/'\n[approval]\nmode='auto'\ntools={ remote_shell='ask' }",
        )
        .unwrap();
        let e = cfg.approval.effective();
        assert_eq!(e["remote_shell"], ApprovalMode::Ask);
        assert_eq!(e["remote_write"], ApprovalMode::Auto);
        assert_eq!(e["remote_read"], ApprovalMode::Auto);
        assert_eq!(e.len(), MUTATING_TOOLS.len() + READ_ONLY_TOOLS.len());

        let strict = Config::parse(
            "[remote]\nhost='h'\nworkdir='/'\n[approval]\nmode='strict'\ntools={ remote_info='auto' }",
        )
        .unwrap();
        let e = strict.approval.effective();
        assert_eq!(e["remote_read"], ApprovalMode::Ask);
        assert_eq!(e["remote_shell"], ApprovalMode::Ask);
        assert_eq!(e["remote_info"], ApprovalMode::Auto);

        let default = Config::parse("[remote]\nhost='h'\nworkdir='/'").unwrap();
        assert_eq!(
            default.approval.effective()["remote_read"],
            ApprovalMode::Auto
        );
        assert!(Config::parse(
            "[remote]\nhost='h'\nworkdir='/'\n[approval]\ntools={ nope='auto' }"
        )
        .is_err());
        assert!(Config::parse(
            "[remote]\nhost='h'\nworkdir='/'\n[approval]\ntools={ upload='strict' }"
        )
        .is_err());
    }

    #[test]
    fn activation_depends_on_source() {
        let mk = |activation: &str, source: Source| Loaded {
            config: Config::parse(&format!(
                "activation='{activation}'\n[remote]\nhost='h'\nworkdir='/'"
            ))
            .unwrap(),
            path: PathBuf::new(),
            source,
        };
        assert!(mk("always", Source::Global).active());
        assert!(!mk("project", Source::Global).active());
        assert!(mk("project", Source::Project).active());
        assert!(mk("project", Source::Explicit).active());
        assert_eq!(
            Config::parse("[remote]\nhost='h'\nworkdir='/'")
                .unwrap()
                .activation,
            ActivationMode::Project
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::parse("[remote]\nhost='h'\nworkdir='/'\nbogus=1").is_err());
    }
}
