//! The MCP surface: every tool a coding agent gets instead of its local
//! bash / read / write / edit / glob / grep, plus the narrow local tools.
//!
//! Each tool does the same four things: resolve, guard, act, audit. A
//! refusal is returned as a tool-level error with the reason, so the model
//! can adapt, and is recorded like every other outcome.

use std::sync::Arc;
use std::time::Instant;

use farhand_core::audit::{Audit, Record};
use farhand_core::config::Config;
use farhand_core::error::Error;
use farhand_core::guard::Guard;
use farhand_core::local::LocalScope;
use farhand_core::remote::{ExecOutput, Remote};
use farhand_core::text::{bounded, numbered_window, shell_quote};
use farhand_core::transfer::{Transfer, TransferReport, DEFAULT_EXCLUDES};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::Deserialize;

const DEFAULT_READ_LINES: usize = 2000;
const DEFAULT_GREP_RESULTS: usize = 200;

#[derive(Clone)]
pub struct FarHand {
    /// `None` only for [`FarHand::inactive`].
    inner: Option<Arc<Inner>>,
    tool_router: ToolRouter<Self>,
    /// False when the config says this directory stays local: the server
    /// then lists no tools and says so in its instructions.
    active: bool,
}

struct Inner {
    remote: Remote,
    guard: Guard,
    local: LocalScope,
    audit: Audit,
    config: Config,
}

// ---- parameters ----------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BashArgs {
    /// The shell command to run on the remote host.
    pub command: String,
    /// Directory to run in (absolute, or relative to the remote workdir).
    pub cwd: Option<String>,
    /// Seconds before the command is killed. Default and maximum come from
    /// the configuration.
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArgs {
    /// Remote file path (absolute, `~/...`, or relative to the workdir).
    pub path: String,
    /// 1-based line to start from.
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteArgs {
    /// Remote file path. Parent directories are created.
    pub path: String,
    /// Full new content of the file.
    pub content: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditArgs {
    /// Remote file path.
    pub path: String,
    /// Exact text to replace. Must appear exactly once unless `replace_all`.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
    /// Replace every occurrence instead of requiring a unique match.
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LsArgs {
    /// Remote directory (default: the workdir).
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GlobArgs {
    /// Glob pattern such as `**/*.rs` or `src/**/*.test.ts`.
    pub pattern: String,
    /// Directory to search (default: the workdir).
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GrepArgs {
    /// Regular expression (ripgrep syntax).
    pub pattern: String,
    /// Directory or file to search (default: the workdir).
    pub path: Option<String>,
    /// Only search files matching this glob, e.g. `*.py`.
    pub include: Option<String>,
    /// Maximum number of matching lines to return (default 200).
    pub max_results: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LocalLsArgs {
    /// Local directory inside the allowlist. Omit to list the allowed roots.
    pub path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LocalReadArgs {
    /// Local file inside the allowlist.
    pub path: String,
    /// 1-based line to start from.
    pub offset: Option<usize>,
    /// Maximum number of lines to return.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UploadArgs {
    /// Local file or directory inside the allowlist.
    pub local_path: String,
    /// Remote destination. For a directory upload, the directory's contents
    /// land inside this path.
    pub remote_path: String,
    /// Extra glob patterns to skip (added to the built-in excludes).
    pub exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadArgs {
    /// Remote file or directory.
    pub remote_path: String,
    /// Local destination inside the allowlist.
    pub local_path: String,
}

// ---- server ---------------------------------------------------------------

#[tool_router]
impl FarHand {
    pub fn new(config: Config, active: bool) -> farhand_core::Result<Self> {
        let guard = Guard::new(&config.guard)?;
        let local = LocalScope::new(&config.local.allowed_dirs);
        let remote = Remote::new(config.remote.clone(), config.limits.clone());
        let audit = Audit::open(config.audit_dir(), &config.remote.host)?;
        Ok(Self {
            inner: Some(Arc::new(Inner {
                remote,
                guard,
                local,
                audit,
                config,
            })),
            tool_router: if active {
                Self::tool_router()
            } else {
                ToolRouter::new()
            },
            active,
        })
    }

    /// A server with no configuration at all: lists no tools, touches
    /// nothing, and says so.
    pub fn inactive() -> Self {
        Self {
            inner: None,
            tool_router: ToolRouter::new(),
            active: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    fn inner(&self) -> &Inner {
        self.inner
            .as_ref()
            .expect("tools are only routed on an active server")
    }

    pub fn instructions(&self) -> String {
        if !self.active {
            return concat!(
                "FarHand is installed but not active in this directory (no .farhand.toml here, ",
                "and no global config with activation = \"always\"). This is an ordinary ",
                "local session: use the normal local tools; FarHand offers none."
            )
            .to_string();
        }
        let c = &self.inner().config;
        let dirs = if self.inner().local.is_empty() {
            "none (the local filesystem is closed)".to_string()
        } else {
            self.inner()
                .local
                .roots()
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let addr = self.inner().remote.address();
        format!(
            "FarHand: all work happens on the remote host `{host}`, in `{workdir}`. \
             The USER is at the local machine, not on the remote. The remote is reachable from \
             the user's machine as `{hostname}` (ssh port {port}); any service you start there \
             listens on the remote, so report it as http://{hostname}:PORT, never as localhost, \
             and if that port may be firewalled offer the tunnel `ssh -L PORT:localhost:PORT \
             {alias}` (then http://localhost:PORT works on the user's machine). Files you create \
             are on the remote unless you `download` them. \
             When the user says \"the current directory\", \"here\", \"this project\", \"list the \
             files\", \"run the tests\" or gives a relative path, they mean that remote workdir — \
             never the local machine. Use remote_ls, remote_read, remote_glob, remote_grep, \
             remote_bash, remote_write and remote_edit for everything unless the user explicitly \
             says local; they replace the local shell and file tools, which are disabled. \
             The local machine is closed except for these directories: {dirs}. Use local_ls and \
             local_read only when the user explicitly asks about those local folders, `upload` to \
             copy from them to the remote, and `download` to copy from the remote into them. \
             Nothing that looks like a credential (keys, tokens, .env files, agent configs) is \
             ever sent to the remote — if a call is refused for that reason, do not work around \
             it. Every action is written to an audit log the user can read.",
            host = c.remote.host,
            workdir = c.remote.workdir,
            hostname = addr.hostname,
            port = addr.port,
            alias = addr.alias,
        )
    }

    pub async fn preflight(&self) -> farhand_core::Result<()> {
        self.inner().remote.connect().await
    }

    // ---- remote tools ----

    #[tool(
        name = "remote_bash",
        description = "Run a shell command on the remote host. Returns stdout, stderr and the exit code. Output is bounded; use head/tail/grep to narrow it. Runs in the remote workdir unless cwd is given."
    )]
    async fn remote_bash(
        &self,
        Parameters(a): Parameters<BashArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_bash");
        rec.command = Some(&a.command);
        if let Err(e) = self
            .inner()
            .guard
            .ensure_content("the command", a.command.as_bytes())
        {
            return Ok(self.deny(rec, started, e));
        }
        match self
            .inner()
            .remote
            .exec(&a.command, a.cwd.as_deref(), a.timeout_secs)
            .await
        {
            Ok(out) => {
                rec.exit_code = Some(out.exit_code);
                rec.outcome = if out.timed_out { "timeout" } else { "ok" };
                self.finish(rec, started);
                Ok(exec_result(&out))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_read",
        description = "Read a file on the remote host with line numbers. Use offset/limit to page through large files."
    )]
    async fn remote_read(
        &self,
        Parameters(a): Parameters<ReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_read");
        rec.path = Some(&a.path);
        let max = self.inner().remote.limits().max_read_bytes;
        match self.inner().remote.read_file(&a.path, max).await {
            Ok((bytes, len)) => {
                rec.bytes = Some(len);
                self.finish(rec, started);
                Ok(read_result(&bytes, len, max, a.offset, a.limit))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_write",
        description = "Create or overwrite a file on the remote host with the given content. Parent directories are created."
    )]
    async fn remote_write(
        &self,
        Parameters(a): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_write");
        rec.path = Some(&a.path);
        rec.bytes = Some(a.content.len() as u64);
        if let Err(e) = self
            .inner()
            .guard
            .ensure_content("the file content", a.content.as_bytes())
        {
            return Ok(self.deny(rec, started, e));
        }
        match self
            .inner()
            .remote
            .write_file(&a.path, a.content.as_bytes())
            .await
        {
            Ok(path) => {
                self.finish(rec, started);
                Ok(ok(format!("Wrote {} bytes to {path}", a.content.len())))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_edit",
        description = "Replace an exact string in a remote file. old_string must match exactly once (whitespace included) unless replace_all is true."
    )]
    async fn remote_edit(
        &self,
        Parameters(a): Parameters<EditArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_edit");
        rec.path = Some(&a.path);
        if let Err(e) = self
            .inner()
            .guard
            .ensure_content("the replacement text", a.new_string.as_bytes())
        {
            return Ok(self.deny(rec, started, e));
        }
        if a.old_string.is_empty() {
            return Ok(self.fail(rec, started, Error::Invalid("old_string is empty".into())));
        }
        let max = self.inner().remote.limits().max_read_bytes;
        let (bytes, len) = match self.inner().remote.read_file(&a.path, max).await {
            Ok(r) => r,
            Err(e) => return Ok(self.fail(rec, started, e)),
        };
        if len as usize > bytes.len() {
            return Ok(self.fail(
                rec,
                started,
                Error::Invalid(format!("file is {len} bytes, above the {max}-byte edit limit; use remote_bash with sed or a script")),
            ));
        }
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                return Ok(self.fail(rec, started, Error::Invalid("file is not UTF-8".into())))
            }
        };
        let count = text.matches(&a.old_string).count();
        let replace_all = a.replace_all.unwrap_or(false);
        let new_text = match count {
            0 => return Ok(self.fail(rec, started, Error::Invalid("old_string not found in file".into()))),
            1 => text.replacen(&a.old_string, &a.new_string, 1),
            _ if replace_all => text.replace(&a.old_string, &a.new_string),
            n => {
                return Ok(self.fail(
                    rec,
                    started,
                    Error::Invalid(format!("old_string matches {n} times; add context to make it unique or set replace_all")),
                ))
            }
        };
        rec.bytes = Some(new_text.len() as u64);
        match self
            .inner()
            .remote
            .write_file(&a.path, new_text.as_bytes())
            .await
        {
            Ok(path) => {
                self.finish(rec, started);
                let n = if replace_all { count } else { 1 };
                Ok(ok(format!("Edited {path}: {n} replacement(s)")))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_ls",
        description = "List a directory on the remote host (ls -la). This is the tool for \"list the files\" / \"what is in the current directory\": with no path it lists the remote workdir."
    )]
    async fn remote_ls(
        &self,
        Parameters(a): Parameters<LsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_ls");
        let path = a.path.unwrap_or_default();
        rec.path = Some(&path);
        let target = if path.is_empty() { "." } else { path.as_str() };
        let cmd = format!("ls -la {}", shell_quote(target));
        match self.inner().remote.exec(&cmd, None, Some(30)).await {
            Ok(out) => {
                rec.exit_code = Some(out.exit_code);
                self.finish(rec, started);
                Ok(exec_result(&out))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_glob",
        description = "Find files on the remote host by glob pattern (e.g. **/*.rs). Returns paths relative to the searched directory."
    )]
    async fn remote_glob(
        &self,
        Parameters(a): Parameters<GlobArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_glob");
        rec.command = Some(&a.pattern);
        let dir = a.path.unwrap_or_default();
        let has_rg = match self.inner().remote.has_tool("rg").await {
            Ok(b) => b,
            Err(e) => return Ok(self.fail(rec, started, e)),
        };
        let cmd = if has_rg {
            format!(
                "rg --files --hidden -g {} -g '!.git' . | head -n 2000",
                shell_quote(&a.pattern)
            )
        } else {
            let name = a.pattern.rsplit('/').next().unwrap_or(&a.pattern);
            format!(
                "find . -path ./.git -prune -o -type f -name {} -print | head -n 2000",
                shell_quote(name)
            )
        };
        match self.inner().remote.exec(&cmd, Some(&dir), Some(60)).await {
            Ok(out) => {
                rec.exit_code = Some(out.exit_code);
                self.finish(rec, started);
                let (text, _) = bounded(&out.stdout, self.inner().remote.limits().max_output_bytes);
                if text.trim().is_empty() {
                    Ok(ok("No files matched."))
                } else {
                    Ok(ok(text))
                }
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_grep",
        description = "Search file contents on the remote host with ripgrep. Returns file:line:text matches."
    )]
    async fn remote_grep(
        &self,
        Parameters(a): Parameters<GrepArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("remote_grep");
        rec.command = Some(&a.pattern);
        let dir = a.path.unwrap_or_default();
        let max = a.max_results.unwrap_or(DEFAULT_GREP_RESULTS).clamp(1, 5000);
        let has_rg = match self.inner().remote.has_tool("rg").await {
            Ok(b) => b,
            Err(e) => return Ok(self.fail(rec, started, e)),
        };
        let include = a
            .include
            .as_deref()
            .map(|g| format!(" -g {}", shell_quote(g)))
            .unwrap_or_default();
        let cmd = if has_rg {
            format!(
                "rg -n --no-heading --color never -S{include} -e {} . | head -n {max}",
                shell_quote(&a.pattern)
            )
        } else {
            let inc = a
                .include
                .as_deref()
                .map(|g| format!(" --include={}", shell_quote(g)))
                .unwrap_or_default();
            format!(
                "grep -rnI -E{inc} --exclude-dir=.git -e {} . | head -n {max}",
                shell_quote(&a.pattern)
            )
        };
        match self.inner().remote.exec(&cmd, Some(&dir), Some(120)).await {
            Ok(out) => {
                rec.exit_code = Some(out.exit_code);
                self.finish(rec, started);
                let (text, _) = bounded(&out.stdout, self.inner().remote.limits().max_output_bytes);
                if text.trim().is_empty() {
                    Ok(ok("No matches."))
                } else {
                    Ok(ok(text))
                }
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    // ---- local tools ----

    #[tool(
        name = "local_ls",
        description = "List a directory on the LOCAL machine — only when the user explicitly asks about their local folders. Only the allowlisted directories are visible; call without a path to see them. For the project / current directory use remote_ls."
    )]
    async fn local_ls(
        &self,
        Parameters(a): Parameters<LocalLsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("local_ls");
        let Some(raw) = a.path else {
            self.finish(rec, started);
            if self.inner().local.is_empty() {
                return Ok(ok("No local directories are allowed."));
            }
            let roots = self
                .inner()
                .local
                .roots()
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ok(format!("Allowed local directories:\n{roots}")));
        };
        rec.path = Some(&raw);
        let dir = match self.inner().local.resolve_existing(&raw) {
            Ok(d) => d,
            Err(e) => return Ok(self.fail(rec, started, local_hint(e))),
        };
        if !dir.is_dir() {
            return Ok(self.fail(
                rec,
                started,
                Error::Invalid(format!("`{}` is not a directory", dir.display())),
            ));
        }
        match self.inner().local.list(&dir) {
            Ok(entries) => {
                self.finish(rec, started);
                let mut s = format!("{}\n", dir.display());
                for e in entries {
                    if e.is_dir {
                        s.push_str(&format!("  {}/\n", e.name));
                    } else {
                        s.push_str(&format!("  {}  ({} bytes)\n", e.name, e.size));
                    }
                }
                Ok(ok(s))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "local_read",
        description = "Read a file on the LOCAL machine — only when the user explicitly asks about their local folders. Allowlisted directories only; project files are on the remote (remote_read)."
    )]
    async fn local_read(
        &self,
        Parameters(a): Parameters<LocalReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("local_read");
        rec.path = Some(&a.path);
        let file = match self.inner().local.resolve_existing(&a.path) {
            Ok(f) => f,
            Err(e) => return Ok(self.fail(rec, started, local_hint(e))),
        };
        let max = self.inner().remote.limits().max_read_bytes;
        let (bytes, len) = match read_local_capped(&file, max) {
            Ok(r) => r,
            Err(e) => return Ok(self.fail(rec, started, e)),
        };
        rec.bytes = Some(len);
        self.finish(rec, started);
        Ok(read_result(&bytes, len, max, a.offset, a.limit))
    }

    #[tool(
        name = "upload",
        description = "Copy a file or directory from an allowlisted LOCAL directory to the remote host. Credential-like files are refused. Directory uploads skip .git, node_modules, target and similar."
    )]
    async fn upload(
        &self,
        Parameters(a): Parameters<UploadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("upload");
        rec.path = Some(&a.local_path);
        rec.target = Some(&a.remote_path);
        let t = self.transfer();
        let excludes = a.exclude.unwrap_or_default();
        match t.upload(&a.local_path, &a.remote_path, &excludes).await {
            Ok(report) => {
                rec.bytes = Some(report.bytes);
                self.finish(rec, started);
                Ok(ok(transfer_summary("Uploaded", &report)))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "download",
        description = "Copy a file or directory from the remote host into an allowlisted LOCAL directory."
    )]
    async fn download(
        &self,
        Parameters(a): Parameters<DownloadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let mut rec = Record::new("download");
        rec.path = Some(&a.remote_path);
        rec.target = Some(&a.local_path);
        let t = self.transfer();
        match t.download(&a.remote_path, &a.local_path).await {
            Ok(report) => {
                rec.bytes = Some(report.bytes);
                self.finish(rec, started);
                Ok(ok(transfer_summary("Downloaded", &report)))
            }
            Err(e) => Ok(self.fail(rec, started, e)),
        }
    }

    #[tool(
        name = "remote_info",
        description = "Show which remote host and workdir FarHand is bound to, the connection state, and the allowlisted local directories."
    )]
    async fn remote_info(&self) -> Result<CallToolResult, McpError> {
        let c = &self.inner().config;
        let connected = self.inner().remote.is_connected().await;
        let workdir = match self.inner().remote.effective_workdir().await {
            Ok(w) if w != c.remote.workdir => format!("{} (= {w})", c.remote.workdir),
            _ => c.remote.workdir.clone(),
        };
        let roots = if self.inner().local.is_empty() {
            "(none)".to_string()
        } else {
            self.inner()
                .local
                .roots()
                .iter()
                .map(|r| format!("  {}", r.display()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let addr = self.inner().remote.address();
        Ok(ok(format!(
            "host: {}\nreachable from the user's machine as: {} (ssh port {})\nworkdir: {}\n\
             connected: {}\nshell: {}\ncommand timeout: {}s (max {}s)\n\
             allowed local directories:\n{}\naudit log: {}\nbuilt-in upload excludes: {}",
            c.remote.host,
            addr.hostname,
            addr.port,
            workdir,
            connected,
            c.remote.shell.join(" "),
            c.limits.command_timeout_secs,
            c.limits.max_command_timeout_secs,
            roots,
            self.inner().audit.dir().display(),
            DEFAULT_EXCLUDES.join(", "),
        )))
    }

    // ---- helpers ----

    fn transfer(&self) -> Transfer<'_> {
        Transfer {
            remote: &self.inner().remote,
            guard: &self.inner().guard,
            local: &self.inner().local,
            limits: &self.inner().config.limits,
        }
    }

    fn finish(&self, mut rec: Record<'_>, started: Instant) {
        rec.duration_ms = started.elapsed().as_millis();
        self.inner().audit.record(rec);
    }

    fn deny(&self, mut rec: Record<'_>, started: Instant, e: Error) -> CallToolResult {
        rec.outcome = "denied";
        // The command may be the very thing the guard caught; the log keeps
        // the fact of the refusal, not the material.
        if rec.command.is_some() {
            rec.command = Some("[redacted: refused by the secret guard]");
        }
        let msg = e.to_string();
        rec.error = Some(msg.clone());
        rec.duration_ms = started.elapsed().as_millis();
        self.inner().audit.record(rec);
        tracing::warn!("{msg}");
        CallToolResult::error(vec![ContentBlock::text(msg)])
    }

    fn fail(&self, mut rec: Record<'_>, started: Instant, e: Error) -> CallToolResult {
        if matches!(e, Error::Denied(_)) {
            return self.deny(rec, started, e);
        }
        rec.outcome = "error";
        let msg = e.to_string();
        rec.error = Some(msg.clone());
        rec.duration_ms = started.elapsed().as_millis();
        self.inner().audit.record(rec);
        tracing::warn!("{msg}");
        CallToolResult::error(vec![ContentBlock::text(msg)])
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FarHand {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(
                "farhand",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(self.instructions())
    }
}

// ---- result shaping ---------------------------------------------------------

/// A local-allowlist refusal usually means the model reached for the local
/// machine when the user meant the project; say so.
fn local_hint(e: Error) -> Error {
    match e {
        Error::Denied(m) => Error::Denied(format!(
            "{m}. If the user meant the project or the current directory, that is on the remote \
             host: use remote_ls / remote_read."
        )),
        other => other,
    }
}

fn ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn exec_result(out: &ExecOutput) -> CallToolResult {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("[stderr]\n");
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    if out.timed_out {
        s.push_str("[command timed out and was killed]\n");
    }
    s.push_str(&format!("[exit code {}]", out.exit_code));
    if out.exit_code == 0 {
        ok(s)
    } else {
        CallToolResult::error(vec![ContentBlock::text(s)])
    }
}

fn read_result(
    bytes: &[u8],
    len: u64,
    max: usize,
    offset: Option<usize>,
    limit: Option<usize>,
) -> CallToolResult {
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return ok(format!("[binary file, {len} bytes]"));
    }
    let text = String::from_utf8_lossy(bytes);
    let (window, total, more) = numbered_window(
        &text,
        offset.unwrap_or(1),
        limit.unwrap_or(DEFAULT_READ_LINES),
    );
    let mut s = window;
    if len as usize > max {
        s.push_str(&format!("\n[showing the first {max} of {len} bytes]"));
    } else if more {
        s.push_str(&format!("\n[{total} lines total; use offset to continue]"));
    }
    if s.is_empty() {
        s = "[empty]".into();
    }
    ok(s)
}

fn read_local_capped(path: &std::path::Path, max: usize) -> farhand_core::Result<(Vec<u8>, u64)> {
    use std::io::Read;
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        return Err(Error::Invalid(format!(
            "`{}` is a directory",
            path.display()
        )));
    }
    let mut f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity((meta.len() as usize).min(max));
    f.by_ref().take(max as u64).read_to_end(&mut buf)?;
    Ok((buf, meta.len()))
}

fn transfer_summary(verb: &str, r: &TransferReport) -> String {
    let mut s = format!(
        "{verb} {} file(s), {} bytes -> {}",
        r.files, r.bytes, r.destination
    );
    if !r.skipped.is_empty() {
        s.push_str(&format!(
            "\nSkipped {}:\n  {}",
            r.skipped.len(),
            r.skipped.join("\n  ")
        ));
    }
    s
}
