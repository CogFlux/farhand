//! Moving files between the allowlisted local directories and the remote.
//! Uploads go through the secret guard file by file; downloads only ever
//! land inside the allowlist.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::Limits;
use crate::error::{Error, Result};
use crate::guard::Guard;
use crate::local::LocalScope;
use crate::remote::Remote;
use crate::text::shell_quote;

/// Directory names skipped by default when uploading a tree. They are
/// either regenerated on the remote or never wanted there.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    ".DS_Store",
    ".idea",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
];

#[derive(Debug, Default)]
pub struct TransferReport {
    pub files: usize,
    pub bytes: u64,
    pub skipped: Vec<String>,
    pub destination: String,
}

pub struct Transfer<'a> {
    pub remote: &'a Remote,
    pub guard: &'a Guard,
    pub local: &'a LocalScope,
    pub limits: &'a Limits,
}

impl Transfer<'_> {
    /// Upload a local file or directory. A file lands at `remote_path`
    /// (or inside it when `remote_path` is an existing directory or ends
    /// with `/`). A directory's *contents* land inside `remote_path`.
    pub async fn upload(
        &self,
        local_raw: &str,
        remote_raw: &str,
        extra_excludes: &[String],
    ) -> Result<TransferReport> {
        let local = self.local.resolve_existing(local_raw)?;
        let remote_base = self.remote.resolve(remote_raw).await?;
        let excludes = build_excludes(extra_excludes)?;
        let mut report = TransferReport::default();

        if local.is_file() {
            let dest = if remote_raw.ends_with('/')
                || self.remote.exists(&remote_base).await? == Some(true)
            {
                let name = local.file_name().unwrap().to_string_lossy();
                format!("{remote_base}/{name}")
            } else {
                remote_base
            };
            let bytes = self.send_file(&local, &dest, &mut report).await?;
            report.bytes = bytes;
            report.destination = dest;
            return Ok(report);
        }
        if !local.is_dir() {
            return Err(Error::Invalid(format!(
                "`{}` is neither a file nor a directory",
                local.display()
            )));
        }

        // Plan first so limits are enforced before anything is sent.
        let mut plan: Vec<(PathBuf, String)> = Vec::new();
        let mut total: u64 = 0;
        let walker = walkdir::WalkDir::new(&local)
            .follow_links(false)
            .into_iter();
        for entry in walker.filter_entry(|e| !is_excluded(e.path(), &local, &excludes)) {
            let entry = entry.map_err(|e| Error::Io(std::io::Error::other(e.to_string())))?;
            let ft = entry.file_type();
            if ft.is_symlink() {
                report
                    .skipped
                    .push(format!("{} (symlink)", entry.path().display()));
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let rel = entry.path().strip_prefix(&local).unwrap();
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            plan.push((
                entry.path().to_path_buf(),
                format!("{remote_base}/{rel_str}"),
            ));
            if plan.len() > self.limits.max_transfer_files {
                return Err(Error::Invalid(format!(
                    "more than {} files; narrow the upload or add excludes",
                    self.limits.max_transfer_files
                )));
            }
        }
        if total > self.limits.max_transfer_bytes {
            return Err(Error::Invalid(format!(
                "{total} bytes exceeds the transfer limit of {} bytes",
                self.limits.max_transfer_bytes
            )));
        }

        // The guard runs over the whole plan before the first byte moves, so a
        // refused file never leaves a half-uploaded tree behind.
        for (path, _) in &plan {
            let bytes = std::fs::read(path)?;
            self.guard.ensure_upload(path, &bytes)?;
        }

        // Create the directory skeleton in one command.
        let mut dirs: Vec<String> = plan
            .iter()
            .filter_map(|(_, dest)| {
                Path::new(dest)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .collect();
        dirs.push(remote_base.clone());
        dirs.sort();
        dirs.dedup();
        let mkdir = format!(
            "mkdir -p {}",
            dirs.iter()
                .map(|d| shell_quote(d))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let out = self.remote.exec(&mkdir, None, Some(60)).await?;
        if out.exit_code != 0 {
            return Err(Error::Ssh(format!(
                "mkdir -p failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }

        for (path, dest) in &plan {
            let bytes = std::fs::read(path)?;
            self.remote.write_file(dest, &bytes).await?;
            report.files += 1;
            report.bytes += bytes.len() as u64;
        }
        report.destination = remote_base;
        Ok(report)
    }

    async fn send_file(
        &self,
        local: &Path,
        dest: &str,
        report: &mut TransferReport,
    ) -> Result<u64> {
        let meta = std::fs::metadata(local)?;
        if meta.len() > self.limits.max_transfer_bytes {
            return Err(Error::Invalid(format!(
                "{} bytes exceeds the transfer limit of {} bytes",
                meta.len(),
                self.limits.max_transfer_bytes
            )));
        }
        let bytes = std::fs::read(local)?;
        self.guard.ensure_upload(local, &bytes)?;
        self.remote.write_file(dest, &bytes).await?;
        report.files += 1;
        Ok(bytes.len() as u64)
    }

    /// Download a remote file or directory into the allowlist. A file lands
    /// at `local_path` (or inside it when that is an existing directory or
    /// ends with `/`); a directory's contents land inside `local_path`.
    pub async fn download(&self, remote_raw: &str, local_raw: &str) -> Result<TransferReport> {
        let remote_path = self.remote.resolve(remote_raw).await?;
        let local = self.local.resolve_for_write(local_raw)?;
        let mut report = TransferReport::default();
        let max = self.limits.max_transfer_bytes as usize;

        match self.remote.exists(&remote_path).await? {
            None => Err(Error::Invalid(format!(
                "`{remote_path}` does not exist on the remote"
            ))),
            Some(false) => {
                let dest = if local.is_dir() || local_raw.ends_with('/') {
                    local.join(Path::new(&remote_path).file_name().unwrap_or_default())
                } else {
                    local
                };
                let dest = self.local_target(&dest)?;
                let (bytes, len) = self.remote.read_file(&remote_path, max).await?;
                if len as usize > max {
                    return Err(Error::Invalid(format!(
                        "`{remote_path}` exceeds the transfer limit"
                    )));
                }
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, &bytes)?;
                report.files = 1;
                report.bytes = bytes.len() as u64;
                report.destination = dest.display().to_string();
                Ok(report)
            }
            Some(true) => {
                let list = self
                    .remote
                    .exec(
                        &format!("find {} -type f -print", shell_quote(&remote_path)),
                        None,
                        Some(120),
                    )
                    .await?;
                if list.exit_code != 0 {
                    return Err(Error::Ssh(
                        String::from_utf8_lossy(&list.stderr).trim().to_string(),
                    ));
                }
                let files: Vec<String> = String::from_utf8_lossy(&list.stdout)
                    .lines()
                    .map(str::to_string)
                    .collect();
                if files.len() > self.limits.max_transfer_files {
                    return Err(Error::Invalid(format!(
                        "more than {} files; narrow the download",
                        self.limits.max_transfer_files
                    )));
                }
                // Names come from the remote, so every destination is checked
                // like a model-supplied path before anything is written:
                // no `..`, no escaping through a local symlink, nothing an
                // editor or agent would trust.
                let mut plan: Vec<(String, PathBuf)> = Vec::with_capacity(files.len());
                for f in files {
                    let rel = f
                        .strip_prefix(&remote_path)
                        .unwrap_or(&f)
                        .trim_start_matches('/');
                    if rel.is_empty() || rel.split('/').any(|seg| seg == "..") {
                        report.skipped.push(f.clone());
                        continue;
                    }
                    let dest = self.local_target(&local.join(rel))?;
                    plan.push((f, dest));
                }
                for (f, dest) in plan {
                    let (bytes, len) = self.remote.read_file(&f, max).await?;
                    if report.bytes + len > self.limits.max_transfer_bytes {
                        return Err(Error::Invalid("transfer limit exceeded".into()));
                    }
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &bytes)?;
                    report.files += 1;
                    report.bytes += bytes.len() as u64;
                }
                report.destination = local.display().to_string();
                Ok(report)
            }
        }
    }
}

impl Transfer<'_> {
    /// Re-check a download destination: it must still resolve inside the
    /// allowlist (a symlink under the root could point elsewhere) and its
    /// path relative to that root must pass the guard.
    fn local_target(&self, dest: &Path) -> Result<PathBuf> {
        let dest = self.local.resolve_for_write(&dest.to_string_lossy())?;
        let rel = self
            .local
            .roots()
            .iter()
            .find_map(|r| dest.strip_prefix(r).ok())
            .unwrap_or(&dest);
        self.guard.ensure_download(rel)?;
        Ok(dest)
    }
}

fn build_excludes(extra: &[String]) -> Result<GlobSet> {
    let mut b = GlobSetBuilder::new();
    for g in DEFAULT_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
    {
        b.add(Glob::new(&g).map_err(|e| Error::Invalid(format!("exclude `{g}`: {e}")))?);
    }
    b.build().map_err(|e| Error::Invalid(e.to_string()))
}

fn is_excluded(path: &Path, root: &Path, set: &GlobSet) -> bool {
    if path == root {
        return false;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    if set.is_match(name.as_ref()) {
        return true;
    }
    path.strip_prefix(root)
        .map(|rel| set.is_match(rel))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_excludes_match_names_anywhere() {
        let set = build_excludes(&["*.log".into()]).unwrap();
        let root = Path::new("/p");
        assert!(is_excluded(Path::new("/p/a/node_modules"), root, &set));
        assert!(is_excluded(Path::new("/p/x.log"), root, &set));
        assert!(!is_excluded(Path::new("/p/src/main.rs"), root, &set));
        assert!(!is_excluded(root, root, &set));
    }
}
