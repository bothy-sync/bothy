//! A remote store in a local directory: for tests, LAN shares and synced folders.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{Error, ErrorKind, Result};
use crate::remote::{PutOutcome, RemoteEntry, RemoteStore};

const TMP_PREFIX: &str = ".tmp-";

/// Stores each logical key as a file under a root directory. Writes go to a temporary file
/// first and are then linked into place, so a file is either complete or absent and an
/// existing file is never replaced.
#[derive(Clone, Debug)]
pub struct FolderRemote {
    root: Arc<PathBuf>,
}

fn io(what: &'static str, e: std::io::Error) -> Error {
    Error::new(ErrorKind::Internal, what).with_source(e)
}

fn check_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.is_ascii()
        && !key.contains('\\')
        && key
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.starts_with(TMP_PREFIX));
    if ok {
        Ok(())
    } else {
        Err(Error::invalid_input(format!("bad remote key {key:?}")))
    }
}

impl FolderRemote {
    /// A store rooted at `root`, created if missing.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io("cannot create remote folder", e))?;
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        check_key(key)?;
        Ok(key
            .split('/')
            .fold(self.root.to_path_buf(), |p, c| p.join(c)))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Path) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || f(&root))
            .await
            .map_err(|e| Error::new(ErrorKind::Internal, "folder task failed").with_source(e))?
    }
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<RemoteEntry>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io("cannot list remote folder", e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| io("cannot list remote folder", e))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(TMP_PREFIX) {
            continue;
        }
        let path = entry.path();
        let meta = entry
            .metadata()
            .map_err(|e| io("cannot stat remote file", e))?;
        if meta.is_dir() {
            walk(root, &path, prefix, out)?;
            continue;
        }
        let rel = path.strip_prefix(root).expect("walk stays under root");
        let key = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if key.starts_with(prefix) {
            out.push(RemoteEntry {
                key,
                size: meta.len(),
            });
        }
    }
    Ok(())
}

#[async_trait]
impl RemoteStore for FolderRemote {
    async fn list(&self, prefix: &str) -> Result<Vec<RemoteEntry>> {
        let prefix = prefix.to_owned();
        self.blocking(move |root| {
            let mut out = Vec::new();
            walk(root, root, &prefix, &mut out)?;
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        })
        .await
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(key)?;
        self.blocking(move |_| match fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io("cannot read remote file", e)),
        })
        .await
    }

    async fn put_new(
        &self,
        key: &str,
        data: &[u8],
        _reservation: Option<&str>,
    ) -> Result<PutOutcome> {
        let path = self.path(key)?;
        let data = data.to_vec();
        self.blocking(move |_| {
            let dir = path.parent().expect("keys have a parent");
            fs::create_dir_all(dir).map_err(|e| io("cannot create remote folder", e))?;
            if let Ok(existing) = fs::read(&path) {
                return Ok(PutOutcome::AlreadyExists {
                    same_content: existing == data,
                });
            }
            let mut suffix = [0u8; 8];
            getrandom::fill(&mut suffix)
                .map_err(|_| Error::new(ErrorKind::Internal, "system random generator failed"))?;
            let tmp = dir.join(format!("{TMP_PREFIX}{}", crate::model::to_hex(&suffix)));
            let result = (|| {
                let mut f =
                    fs::File::create_new(&tmp).map_err(|e| io("cannot write remote file", e))?;
                f.write_all(&data)
                    .map_err(|e| io("cannot write remote file", e))?;
                f.sync_all()
                    .map_err(|e| io("cannot write remote file", e))?;
                drop(f);
                // A hard link fails if the target exists, so a concurrent writer can't be replaced.
                match fs::hard_link(&tmp, &path) {
                    Ok(()) => Ok(PutOutcome::Created),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        let existing =
                            fs::read(&path).map_err(|e| io("cannot read remote file", e))?;
                        Ok(PutOutcome::AlreadyExists {
                            same_content: existing == data,
                        })
                    }
                    Err(e) => Err(io("cannot place remote file", e)),
                }
            })();
            let _ = fs::remove_file(&tmp);
            result
        })
        .await
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        self.blocking(move |_| match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io("cannot delete remote file", e)),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_list_delete() {
        let dir = tempfile::tempdir().unwrap();
        let r = FolderRemote::new(dir.path()).unwrap();
        assert_eq!(
            r.put_new("db/a/one", b"1", None).await.unwrap(),
            PutOutcome::Created
        );
        assert_eq!(
            r.put_new("db/b/two", b"22", None).await.unwrap(),
            PutOutcome::Created
        );
        assert_eq!(r.get("db/a/one").await.unwrap().unwrap(), b"1");
        assert_eq!(r.get("db/a/none").await.unwrap(), None);
        let keys: Vec<_> = r
            .list("db/")
            .await
            .unwrap()
            .into_iter()
            .map(|e| (e.key, e.size))
            .collect();
        assert_eq!(
            keys,
            vec![("db/a/one".to_string(), 1), ("db/b/two".to_string(), 2)]
        );
        assert_eq!(r.list("db/b").await.unwrap().len(), 1);
        r.delete("db/a/one").await.unwrap();
        r.delete("db/a/one").await.unwrap();
        assert_eq!(r.get("db/a/one").await.unwrap(), None);
    }

    #[tokio::test]
    async fn never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let r = FolderRemote::new(dir.path()).unwrap();
        r.put_new("k/x", b"same", None).await.unwrap();
        assert_eq!(
            r.put_new("k/x", b"same", None).await.unwrap(),
            PutOutcome::AlreadyExists { same_content: true }
        );
        assert_eq!(
            r.put_new("k/x", b"other", None).await.unwrap(),
            PutOutcome::AlreadyExists {
                same_content: false
            }
        );
        assert_eq!(r.get("k/x").await.unwrap().unwrap(), b"same");
    }

    #[tokio::test]
    async fn temp_files_are_hidden_and_bad_keys_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let r = FolderRemote::new(dir.path()).unwrap();
        fs::create_dir_all(dir.path().join("k")).unwrap();
        fs::write(dir.path().join("k").join(".tmp-abc"), b"partial").unwrap();
        assert!(r.list("").await.unwrap().is_empty());
        for bad in [
            "", "/abs", "a//b", "../up", "a/./b", "a\\b", "k/.tmp-x", "ü",
        ] {
            assert!(r.get(bad).await.is_err(), "{bad:?}");
        }
    }
}
