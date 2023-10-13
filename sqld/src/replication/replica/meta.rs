use std::io::{ErrorKind, SeekFrom};
use std::mem::size_of;
use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use bytemuck::{bytes_of, try_pod_read_unaligned, Pod, Zeroable};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::pin;
use uuid::Uuid;

use crate::{replication::FrameNo, rpc::replication_log::rpc::HelloResponse};

use super::error::ReplicationError;

#[repr(C)]
#[derive(Debug, Pod, Zeroable, Clone, Copy)]
pub struct WalIndexMetaData {
    /// id of the replicated log
    log_id: u128,
    /// committed frame index
    pub committed_frame_no: FrameNo,
    _padding: u64,
}

impl WalIndexMetaData {
    async fn read(file: impl AsyncRead) -> crate::Result<Option<Self>> {
        pin!(file);
        let mut buf = [0; size_of::<WalIndexMetaData>()];
        let meta = match file.read_exact(&mut buf).await {
            Ok(_) => {
                let meta: Self = try_pod_read_unaligned(&buf)
                    .map_err(|_| anyhow::anyhow!("invalid index meta file"))?;
                Some(meta)
            }
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => None,
            Err(e) => Err(e)?,
        };

        Ok(meta)
    }
}

pub struct WalIndexMeta {
    file: File,
    data: Option<WalIndexMetaData>,
}

impl WalIndexMeta {
    pub async fn open(db_path: &Path) -> crate::Result<Self> {
        let path = db_path.join("client_wal_index");

        tokio::fs::create_dir_all(db_path).await?;

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .await?;

        let data = WalIndexMetaData::read(&mut file).await?;

        Ok(Self { file, data })
    }

    /// attempts to merge two meta files.
    pub fn merge_hello(&mut self, hello: HelloResponse) -> Result<(), ReplicationError> {
        let hello_log_id = Uuid::from_str(&hello.log_id)
            .context("invalid database id from primary")?
            .as_u128();

        match self.data {
            Some(meta) => {
                if meta.log_id != hello_log_id {
                    Err(ReplicationError::LogIncompatible)
                } else {
                    Ok(())
                }
            }
            None => {
                self.data = Some(WalIndexMetaData {
                    log_id: hello_log_id,
                    committed_frame_no: FrameNo::MAX,
                    _padding: 0,
                });
                Ok(())
            }
        }
    }

    pub async fn flush(&mut self) -> crate::Result<()> {
        if let Some(data) = self.data {
            // FIXME: we can save a syscall by calling read_exact_at, but let's use tokio API for now
            self.file.seek(SeekFrom::Start(0)).await?;
            let s = self.file.write(bytes_of(&data)).await?;
            // WalIndexMeta is smaller than a page size, and aligned at the beginning of the file, if
            // should always be written in a single call
            assert_eq!(s, size_of::<WalIndexMetaData>());
            self.file.flush().await?;
        }

        Ok(())
    }

    /// Apply the last commit frame no to the meta file.
    /// This function must be called after each injection, because it's idempotent to re-apply the
    /// last transaction, but not idempotent if we lose track of more than one.
    pub async fn set_commit_frame_no(&mut self, commit_fno: FrameNo) -> crate::Result<()> {
        {
            let data = self
                .data
                .as_mut()
                .expect("call set_commit_frame_no before initializing meta");
            data.committed_frame_no = commit_fno;
        }

        self.flush().await?;

        Ok(())
    }

    pub(crate) fn current_frame_no(&self) -> Option<FrameNo> {
        self.data.and_then(|d| {
            if d.committed_frame_no == FrameNo::MAX {
                None
            } else {
                Some(d.committed_frame_no)
            }
        })
    }
}
