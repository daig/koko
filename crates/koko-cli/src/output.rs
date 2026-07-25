//! Fallible output routing and atomic sibling-file transactions.

use crate::bootstrap::Format;
use std::fs::{File, Metadata};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionPolicy {
    Refuse,
    Replace,
    Append,
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("output destination `{0}` already exists")]
    Exists(PathBuf),
    #[error("output destination `{0}` is a symbolic link")]
    Symlink(PathBuf),
    #[error("output destination `{0}` changed while output was staged")]
    Changed(PathBuf),
    #[error("cannot append a nonempty JSON document; select JSONL")]
    JsonAppend,
    #[error("output parent for `{0}` is not a directory")]
    Parent(PathBuf),
    #[error("cannot publish output `{path}`: {source}")]
    Publish {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl FileIdentity {
    fn capture(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        }
    }
}

/// Whole-invocation or one-submission atomic output staging.
pub struct OutputTransaction {
    destination: PathBuf,
    temporary: Option<NamedTempFile>,
    policy: CollisionPolicy,
    initial_identity: Option<FileIdentity>,
}

impl OutputTransaction {
    pub fn begin(
        destination: &Path,
        policy: CollisionPolicy,
        format: Format,
    ) -> Result<Self, OutputError> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(OutputError::Parent(destination.to_path_buf()));
        }
        let initial = destination_metadata(destination)?;
        if initial.is_some() && policy == CollisionPolicy::Refuse {
            return Err(OutputError::Exists(destination.to_path_buf()));
        }
        if policy == CollisionPolicy::Append
            && format == Format::Json
            && initial.as_ref().is_some_and(|metadata| metadata.len() != 0)
        {
            return Err(OutputError::JsonAppend);
        }
        let initial_identity = initial.as_ref().map(FileIdentity::capture);
        let mut temporary = tempfile::Builder::new()
            .prefix(".koko-output-")
            .tempfile_in(parent)?;
        if policy == CollisionPolicy::Append && initial.is_some() {
            let mut existing = File::open(destination)?;
            std::io::copy(&mut existing, temporary.as_file_mut())?;
        }
        Ok(Self {
            destination: destination.to_path_buf(),
            temporary: Some(temporary),
            policy,
            initial_identity,
        })
    }

    pub fn commit(mut self) -> Result<(), OutputError> {
        let current = destination_metadata(&self.destination)?;
        if current.as_ref().map(FileIdentity::capture) != self.initial_identity {
            return Err(OutputError::Changed(self.destination.clone()));
        }
        let mut temporary = self.temporary.take().expect("uncommitted transaction");
        temporary.as_file_mut().flush()?;
        temporary.as_file().sync_all()?;
        let path = self.destination.clone();
        let persisted = if self.policy == CollisionPolicy::Refuse {
            temporary.persist_noclobber(&path)
        } else {
            temporary.persist(&path)
        };
        match persisted {
            Ok(_) => Ok(()),
            Err(error) => Err(OutputError::Publish {
                path,
                source: error.error,
            }),
        }
    }

    pub fn staged_len(&self) -> Result<u64, OutputError> {
        Ok(self
            .temporary
            .as_ref()
            .expect("uncommitted transaction")
            .as_file()
            .metadata()?
            .len())
    }
}

impl Write for OutputTransaction {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.temporary
            .as_mut()
            .expect("uncommitted transaction")
            .write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.temporary
            .as_mut()
            .expect("uncommitted transaction")
            .flush()
    }
}

fn destination_metadata(destination: &Path) -> Result<Option<Metadata>, OutputError> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(OutputError::Symlink(destination.to_path_buf()))
        }
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OutputError::Io(error)),
    }
}

/// Copy a bounded spill to its selected data destination.
pub fn publish_spill(
    spill: &mut tempfile::SpooledTempFile,
    destination: &mut impl Write,
) -> std::io::Result<u64> {
    spill.seek(SeekFrom::Start(0))?;
    std::io::copy(spill, destination)
}
