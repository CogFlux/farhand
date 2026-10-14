//! The local allowlist. The model sees the local filesystem only through
//! this module, and this module only ever resolves paths inside the
//! directories the user listed in `local.allowed_dirs`.
//!
//! A path is checked once, when it is resolved, and then reached from a
//! handle on its allowed root one component at a time without following a
//! symlink (`openat` with `O_NOFOLLOW`). Something that swaps a symlink in
//! between the check and the access — a sync tool, an editor, another
//! process — gets a refusal, not a file outside the allowlist.

use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::config::expand_home;
use crate::error::{Error, Result};

pub struct LocalScope {
    roots: Vec<PathBuf>,
    /// `(dev, ino)` of each root when the scope was made; `None` for a root
    /// that did not exist then. A root replaced since is refused.
    ids: Vec<Option<(u64, u64)>>,
}

#[derive(Debug, Clone)]
pub struct LocalEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

impl LocalScope {
    /// Roots are canonicalised now so symlinked roots still compare equal
    /// to canonicalised requests, and their identity is recorded. A root
    /// that does not exist yet is kept as written and simply never matches.
    pub fn new(allowed: &[PathBuf]) -> Self {
        let mut roots = Vec::with_capacity(allowed.len());
        let mut ids = Vec::with_capacity(allowed.len());
        for p in allowed {
            let p = expand_home(p);
            let (root, id) = match p.canonicalize() {
                Ok(c) => {
                    let id = rustix::fs::stat(&c).ok().map(|st| identity(&st));
                    (c, id)
                }
                Err(_) => (p, None),
            };
            roots.push(root);
            ids.push(id);
        }
        Self { roots, ids }
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
        let p = self.absolute(raw)?;
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
        let p = self.absolute(raw)?;
        if p.components().any(|c| matches!(c, Component::ParentDir)) {
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

    /// `raw` as an absolute path: relative paths are taken against the one
    /// root, and are ambiguous when there are several.
    fn absolute(&self, raw: &str) -> Result<PathBuf> {
        if self.roots.is_empty() {
            return Err(Error::Denied(
                "no local directories are allowed (local.allowed_dirs is empty)".into(),
            ));
        }
        let p = expand_home(Path::new(raw));
        if p.is_absolute() {
            Ok(p)
        } else if self.roots.len() == 1 {
            Ok(self.roots[0].join(p))
        } else {
            Err(Error::Invalid(format!(
                "`{raw}` is relative but several local directories are allowed; use an absolute \
                 path"
            )))
        }
    }

    fn ensure_inside(&self, canon: &Path) -> Result<()> {
        if self.roots.iter().any(|r| canon.starts_with(r)) {
            Ok(())
        } else {
            Err(self.outside(canon))
        }
    }

    fn outside(&self, path: &Path) -> Error {
        Error::Denied(format!(
            "`{}` is outside the allowed local directories ({})",
            path.display(),
            self.roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// Resolve and open a regular file for reading.
    pub fn open_file(&self, raw: &str) -> Result<(PathBuf, std::fs::File)> {
        let canon = self.resolve_existing(raw)?;
        let file = self.open_resolved(&canon)?;
        Ok((canon, file))
    }

    /// Open the regular file at `path`, a path inside a root as
    /// [`resolve_existing`](Self::resolve_existing) or a walk below one
    /// returns it, without following a symlink anywhere below the root.
    pub fn open_resolved(&self, path: &Path) -> Result<std::fs::File> {
        let (dir, name) = self.walk(path, false)?;
        let Some(name) = name else {
            return Err(Error::Invalid(format!(
                "`{}` is a directory",
                path.display()
            )));
        };
        // `O_NONBLOCK` so a FIFO cannot hang the open; it is refused below.
        let fd = rustix::fs::openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(|e| refused(&dir, name, path, e))?;
        let st = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
        match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile => Ok(std::fs::File::from(fd)),
            FileType::Directory => Err(Error::Invalid(format!(
                "`{}` is a directory",
                path.display()
            ))),
            _ => Err(Error::Invalid(format!(
                "`{}` is not a regular file",
                path.display()
            ))),
        }
    }

    /// List the directory at `dir`, a path inside a root.
    pub fn list(&self, dir: &Path) -> Result<Vec<LocalEntry>> {
        let (parent, name) = self.walk(dir, false)?;
        let fd = match name {
            None => parent,
            Some(n) => open_dir_at(&parent, n).map_err(|e| refused(&parent, n, dir, e))?,
        };
        let mut out = Vec::new();
        for entry in Dir::read_from(&fd).map_err(std::io::Error::from)? {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            // An entry removed since the directory was read is skipped.
            let Ok(st) = rustix::fs::statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            out.push(LocalEntry {
                name: OsStr::from_bytes(name.to_bytes())
                    .to_string_lossy()
                    .into_owned(),
                is_dir: FileType::from_raw_mode(st.st_mode) == FileType::Directory,
                size: st.st_size as u64,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Start a download to `dest` (as [`resolve_for_write`](Self::resolve_for_write)
    /// returns it): missing folders are created, and the data goes to a
    /// randomly named temporary file created exclusively in the destination
    /// folder. [`PartFile::commit`] renames it into place.
    pub fn create_part(&self, dest: &Path) -> Result<PartFile> {
        let (dir, name) = self.walk(dest, true)?;
        let Some(name) = name else {
            return Err(Error::Invalid(format!(
                "`{}` is a directory",
                dest.display()
            )));
        };
        let part = OsString::from(format!(
            ".{}.{:016x}.farhand-part",
            name.to_string_lossy(),
            crate::remote::random_u64()
        ));
        let fd = rustix::fs::openat(
            &dir,
            &part,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )
        .map_err(|e| {
            Error::Io(std::io::Error::new(
                std::io::Error::from(e).kind(),
                format!(
                    "cannot create a temporary file next to {}: {e}",
                    dest.display()
                ),
            ))
        })?;
        Ok(PartFile {
            dir,
            part,
            name: name.to_owned(),
            file: Arc::new(std::fs::File::from(fd)),
            dest: dest.to_path_buf(),
            committed: false,
        })
    }

    /// The handle on the folder holding `path` and the last component of
    /// `path` (`None` when `path` is the root itself). Every folder on the
    /// way is opened from the one above it without following a symlink;
    /// with `create`, missing ones are made.
    fn walk<'p>(&self, path: &'p Path, create: bool) -> Result<(OwnedFd, Option<&'p OsStr>)> {
        // A root that existed at startup is preferred, so a nested one
        // that did not cannot shadow it.
        let found = |known: bool| {
            self.roots.iter().enumerate().find_map(|(i, r)| {
                (self.ids[i].is_some() == known).then_some(())?;
                Some((i, path.strip_prefix(r).ok()?))
            })
        };
        let Some((i, rel)) = found(true).or_else(|| found(false)) else {
            return Err(self.outside(path));
        };
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(n) => names.push(n),
                _ => {
                    return Err(Error::Invalid(format!(
                        "`{}` is not a resolved path",
                        path.display()
                    )))
                }
            }
        }
        let mut dir = self.open_root(i)?;
        let Some((last, parents)) = names.split_last() else {
            return Ok((dir, None));
        };
        for n in parents {
            dir = match open_dir_at(&dir, n) {
                Ok(d) => d,
                Err(Errno::NOENT) if create => {
                    match rustix::fs::mkdirat(&dir, *n, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(Error::Io(e.into())),
                    }
                    open_dir_at(&dir, n).map_err(|e| refused(&dir, n, path, e))?
                }
                Err(e) => return Err(refused(&dir, n, path, e)),
            };
        }
        Ok((dir, Some(last)))
    }

    fn open_root(&self, i: usize) -> Result<OwnedFd> {
        let root = &self.roots[i];
        let gone = || {
            Error::Denied(format!(
                "the allowed folder `{}` did not exist or was replaced after FarHand started; \
                 restart the agent to pick it up",
                root.display()
            ))
        };
        let expected = self.ids[i].ok_or_else(gone)?;
        let fd = rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| gone())?;
        let st = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
        if identity(&st) != expected {
            return Err(gone());
        }
        Ok(fd)
    }
}

/// A download in progress: a temporary file in its destination folder,
/// held by a handle on that folder. Dropped without
/// [`commit`](Self::commit), it is removed.
pub struct PartFile {
    dir: OwnedFd,
    part: OsString,
    name: OsString,
    file: Arc<std::fs::File>,
    dest: PathBuf,
    committed: bool,
}

impl PartFile {
    pub fn file(&self) -> &Arc<std::fs::File> {
        &self.file
    }

    pub fn dest(&self) -> &Path {
        &self.dest
    }

    /// Flush the data and rename it into place, within the one folder
    /// handle: the file lands in the folder it was written in, and a
    /// symlink at the destination is replaced rather than written through.
    pub fn commit(mut self) -> Result<()> {
        self.file.sync_all()?;
        if rustix::fs::statat(&self.dir, &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Directory)
        {
            return Err(Error::Invalid(format!(
                "`{}` is a directory",
                self.dest.display()
            )));
        }
        rustix::fs::renameat(&self.dir, &self.part, &self.dir, &self.name)
            .map_err(std::io::Error::from)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = rustix::fs::unlinkat(&self.dir, &self.part, AtFlags::empty());
        }
    }
}

// The field types differ between platforms (`st_dev` is `i32` on macOS).
#[allow(clippy::unnecessary_cast)]
fn identity(st: &rustix::fs::Stat) -> (u64, u64) {
    (st.st_dev as u64, st.st_ino as u64)
}

fn open_dir_at(dir: &OwnedFd, name: &OsStr) -> std::result::Result<OwnedFd, Errno> {
    rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// The error for a component that would not open: a refusal if a symlink
/// now stands where the resolved path had none, the I/O error otherwise.
fn refused(dir: &OwnedFd, name: &OsStr, path: &Path, e: Errno) -> Error {
    let is_link = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)
        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink);
    if is_link {
        Error::Denied(format!(
            "`{}` changed while it was being opened: `{}` is now a symlink",
            path.display(),
            name.to_string_lossy()
        ))
    } else {
        let e = std::io::Error::from(e);
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::symlink;

    #[test]
    fn refuses_outside_and_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("allowed");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        std::fs::write(root.join("ok.txt"), "x").unwrap();
        symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();

        let scope = LocalScope::new(std::slice::from_ref(&root));
        assert!(scope
            .resolve_existing(root.join("ok.txt").to_str().unwrap())
            .is_ok());
        assert!(scope.resolve_existing("ok.txt").is_ok());
        assert!(scope
            .resolve_existing(outside.join("secret.txt").to_str().unwrap())
            .is_err());
        assert!(scope.resolve_existing("../outside/secret.txt").is_err());
        assert!(scope.resolve_existing("link.txt").is_err());
        assert!(scope.open_file("link.txt").is_err());
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

    /// A tree with an allowed root holding `sub/file.txt` and a sibling
    /// `outside/` holding `secret.txt`.
    fn tree() -> (tempfile::TempDir, PathBuf, PathBuf, LocalScope) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap().join("allowed");
        let outside = tmp.path().canonicalize().unwrap().join("outside");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("sub/file.txt"), "inside").unwrap();
        std::fs::write(outside.join("file.txt"), "secret").unwrap();
        let scope = LocalScope::new(std::slice::from_ref(&root));
        (tmp, root, outside, scope)
    }

    #[test]
    fn a_file_swapped_for_a_link_after_the_check_is_refused() {
        let (_t, root, outside, scope) = tree();
        let (path, mut f) = scope.open_file("sub/file.txt").unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "inside");

        // The check passed; then the file becomes a link out.
        std::fs::remove_file(root.join("sub/file.txt")).unwrap();
        symlink(outside.join("file.txt"), root.join("sub/file.txt")).unwrap();
        let err = scope.open_resolved(&path).unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err}");
    }

    #[test]
    fn a_folder_swapped_for_a_link_after_the_check_is_refused() {
        let (_t, root, outside, scope) = tree();
        let path = scope.resolve_existing("sub/file.txt").unwrap();
        std::fs::rename(root.join("sub"), root.join("old")).unwrap();
        symlink(&outside, root.join("sub")).unwrap();
        let err = scope.open_resolved(&path).unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err}");
        let err = scope.list(&root.join("sub")).unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err}");

        // A download through it is refused before anything is created.
        let dest = root.join("sub/new.txt");
        assert!(matches!(scope.create_part(&dest), Err(Error::Denied(_))));
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
    }

    #[test]
    fn a_replaced_root_is_refused() {
        let (_t, root, _outside, scope) = tree();
        std::fs::rename(&root, root.with_extension("old")).unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file.txt"), "new").unwrap();
        let err = scope.open_file("sub/file.txt").unwrap_err();
        assert!(matches!(err, Error::Denied(_)), "{err}");
    }

    #[test]
    fn listing_does_not_follow_links() {
        let (_t, root, outside, scope) = tree();
        symlink(&outside, root.join("out")).unwrap();
        let entries = scope.list(&root).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["out", "sub"]);
        assert!(!entries[0].is_dir);
        assert!(scope.open_file("out").is_err());
        assert!(scope.open_file("sub").is_err());
    }

    #[test]
    fn downloads_land_in_place_and_replace_a_planted_link() {
        let (_t, root, outside, scope) = tree();
        // New folders are created on the way.
        let dest = scope.resolve_for_write("a/b/new.txt").unwrap();
        let part = scope.create_part(&dest).unwrap();
        (&**part.file()).write_all(b"data").unwrap();
        part.commit().unwrap();
        assert_eq!(std::fs::read(root.join("a/b/new.txt")).unwrap(), b"data");

        // A link at the destination is replaced, its target untouched.
        symlink(outside.join("file.txt"), root.join("sub/link.txt")).unwrap();
        let part = scope.create_part(&root.join("sub/link.txt")).unwrap();
        (&**part.file()).write_all(b"new").unwrap();
        part.commit().unwrap();
        assert_eq!(std::fs::read(outside.join("file.txt")).unwrap(), b"secret");
        assert!(!std::fs::symlink_metadata(root.join("sub/link.txt"))
            .unwrap()
            .file_type()
            .is_symlink());

        // An abandoned download leaves nothing behind.
        let part = scope.create_part(&root.join("sub/gone.txt")).unwrap();
        drop(part);
        let names: Vec<_> = std::fs::read_dir(root.join("sub"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
    }
}
