//! The local allowlist. The model sees the local filesystem only through
//! this module, and this module only ever resolves paths inside the
//! directories the user listed in `local.allowed_dirs`.

use std::path::{Path, PathBuf};

use crate::config::expand_home;
use crate::error::{Error, Result};

pub struct LocalScope {
    roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl LocalScope {
    /// Roots are canonicalised now so symlinked roots still compare equal
    /// to canonicalised requests. A root that does not exist yet is kept
    /// as written and simply never matches.
    pub fn new(allowed: &[PathBuf]) -> Self {
        let roots = allowed
            .iter()
            .map(|p| {
                let p = expand_home(p);
                p.canonicalize().unwrap_or(p)
            })
            .collect();
        Self { roots }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Resolve a model-supplied path to a canonical path inside a root.
    /// Symlinks are followed *before* the containment check, so a link
    /// pointing outside the allowed directories is refused.
    pub fn resolve_existing(&self, raw: &str) -> Result<PathBuf> {
        if self.roots.is_empty() {
            return Err(Error::Denied(
                "no local directories are allowed (local.allowed_dirs is empty)".into(),
            ));
        }
        let p = expand_home(Path::new(raw));
        let p = if p.is_absolute() {
            p
        } else if self.roots.len() == 1 {
            self.roots[0].join(p)
        } else {
            return Err(Error::Invalid(format!(
                "`{raw}` is relative but several local directories are allowed; use an absolute \
                 path"
            )));
        };
        let canon = p.canonicalize().map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("{}: {e}", p.display()),
            ))
        })?;
        self.ensure_inside(&canon)?;
        Ok(canon)
    }

    /// Resolve a path that may not exist yet (a download target). The
    /// nearest existing ancestor must be inside a root.
    pub fn resolve_for_write(&self, raw: &str) -> Result<PathBuf> {
        if self.roots.is_empty() {
            return Err(Error::Denied(
                "no local directories are allowed (local.allowed_dirs is empty)".into(),
            ));
        }
        let p = expand_home(Path::new(raw));
        let p = if p.is_absolute() {
            p
        } else if self.roots.len() == 1 {
            self.roots[0].join(p)
        } else {
            return Err(Error::Invalid(format!(
                "`{raw}` is relative but several local directories are allowed; use an absolute \
                 path"
            )));
        };
        if p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::Denied(format!("`{raw}` contains `..`")));
        }
        let mut existing = p.as_path();
        let mut tail = Vec::new();
        loop {
            if existing.exists() {
                break;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    tail.push(name.to_owned());
                    existing = parent;
                }
                _ => return Err(Error::Invalid(format!("`{raw}` has no existing ancestor"))),
            }
        }
        let mut canon = existing.canonicalize()?;
        self.ensure_inside(&canon)?;
        for name in tail.into_iter().rev() {
            canon.push(name);
        }
        Ok(canon)
    }

    fn ensure_inside(&self, canon: &Path) -> Result<()> {
        if self.roots.iter().any(|r| canon.starts_with(r)) {
            Ok(())
        } else {
            Err(Error::Denied(format!(
                "`{}` is outside the allowed local directories ({})",
                canon.display(),
                self.roots
                    .iter()
                    .map(|r| r.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }

    pub fn list(&self, dir: &Path) -> Result<Vec<LocalEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            out.push(LocalEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_outside_and_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("allowed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        std::fs::write(root.join("ok.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();

        let scope = LocalScope::new(std::slice::from_ref(&root));
        assert!(scope
            .resolve_existing(root.join("ok.txt").to_str().unwrap())
            .is_ok());
        assert!(scope.resolve_existing("ok.txt").is_ok());
        assert!(scope
            .resolve_existing(outside.join("secret.txt").to_str().unwrap())
            .is_err());
        assert!(scope.resolve_existing("../outside/secret.txt").is_err());
        #[cfg(unix)]
        assert!(scope.resolve_existing("link.txt").is_err());
    }

    #[test]
    fn write_target_may_not_exist_yet() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("allowed");
        std::fs::create_dir_all(&root).unwrap();
        let scope = LocalScope::new(std::slice::from_ref(&root));
        let p = scope.resolve_for_write("new/dir/file.bin").unwrap();
        assert!(p.starts_with(root.canonicalize().unwrap()));
        assert!(scope.resolve_for_write("../x").is_err());
        assert!(scope.resolve_for_write("/etc/passwd").is_err());
    }

    #[test]
    fn empty_scope_denies_everything() {
        let scope = LocalScope::new(&[]);
        assert!(matches!(
            scope.resolve_existing("/tmp"),
            Err(Error::Denied(_))
        ));
    }
}
