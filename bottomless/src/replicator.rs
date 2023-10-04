use crate::backup::WalCopier;
use crate::read::BatchReader;
use crate::s3::{S3Client, WalSegmentSummary};
use crate::transaction_cache::TransactionPageCache;
use crate::uuid_utils::GenerationUuid;
use crate::wal::WalFileReader;
use anyhow::{anyhow, bail};
use arc_swap::ArcSwapOption;
use async_compression::tokio::write::GzipEncoder;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client, Config};
use bytes::Bytes;
use chrono::{NaiveDateTime, Utc};
use std::io::SeekFrom;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch::{channel, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::task::JoinSet;
use tokio::time::Duration;
use tokio::time::{timeout_at, Instant};
use uuid::Uuid;

/// Maximum number of generations that can participate in database restore procedure.
/// This effectively means that at least one in [MAX_RESTORE_STACK_DEPTH] number of
/// consecutive generations has to have a snapshot included.
const MAX_RESTORE_STACK_DEPTH: usize = 100;

pub type Result<T> = anyhow::Result<T>;

#[derive(Debug)]
pub struct Replicator {
    pub client: S3Client,

    /// Frame number, incremented whenever a new frame is written from SQLite.
    next_frame_no: Arc<AtomicU32>,
    /// Last frame which has been requested to be sent to S3.
    /// Always: [last_sent_frame_no] <= [next_frame_no].
    last_sent_frame_no: Arc<AtomicU32>,
    /// Last frame which has been confirmed as stored locally outside of WAL file.
    /// Always: [last_committed_frame_no] <= [last_sent_frame_no].
    last_committed_frame_no: Receiver<Result<u32>>,
    flush_trigger: Sender<()>,
    snapshot_waiter: Receiver<Result<Option<Uuid>>>,
    snapshot_notifier: Arc<Sender<Result<Option<Uuid>>>>,
    snapshot_interval: Option<Duration>,

    pub page_size: usize,
    restore_transaction_page_swap_after: u32,
    restore_transaction_cache_fpath: Arc<str>,
    generation: Arc<ArcSwapOption<Uuid>>,
    verify_crc: bool,
    pub bucket: String,
    pub db_path: String,
    pub db_name: String,

    use_compression: CompressionKind,
    max_frames_per_batch: usize,
    s3_upload_max_parallelism: usize,
    _join_set: JoinSet<()>,
}

#[derive(Debug)]
pub struct FetchedResults {
    pub pages: Vec<(i32, Bytes)>,
    pub next_marker: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RestoreAction {
    SnapshotMainDbFile,
    ReuseGeneration(Uuid),
}

#[derive(Clone, Debug)]
pub struct Options {
    pub create_bucket_if_not_exists: bool,
    /// If `true` when restoring, frames checksums will be verified prior their pages being flushed
    /// into the main database file.
    pub verify_crc: bool,
    /// Kind of compression algorithm used on the WAL frames to be sent to S3.
    pub use_compression: CompressionKind,
    pub aws_endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub region: Option<String>,
    pub db_id: Option<String>,
    /// Bucket directory name where all S3 objects are backed up. General schema is:
    /// - `{db-name}-{uuid-v7}` subdirectories:
    ///   - `.meta` file with database page size and initial WAL checksum.
    ///   - Series of files `{first-frame-no}-{last-frame-no}.{compression-kind}` containing
    ///     the batches of frames from which the restore will be made.
    pub bucket_name: String,
    /// Max number of WAL frames per S3 object.
    pub max_frames_per_batch: usize,
    /// Max time before next frame of batched frames should be synced. This works in the case
    /// when we don't explicitly run into `max_frames_per_batch` threshold and the corresponding
    /// checkpoint never commits.
    pub max_batch_interval: Duration,
    /// Maximum number of S3 file upload requests that may happen in parallel.
    pub s3_upload_max_parallelism: usize,
    /// When recovering a transaction, if number of affected pages is greater than page swap,
    /// start flushing these pages on disk instead of keeping them in memory.
    pub restore_transaction_page_swap_after: u32,
    /// When recovering a transaction, when its page cache needs to be swapped onto local file,
    /// this field contains a path for a file to be used.
    pub restore_transaction_cache_fpath: String,
    pub snapshot_interval: Option<Duration>,
}

impl Options {
    pub async fn client_config(&self) -> Result<Config> {
        let mut loader = aws_config::from_env();
        if let Some(endpoint) = self.aws_endpoint.as_deref() {
            loader = loader.endpoint_url(endpoint);
        }
        let region = self
            .region
            .clone()
            .ok_or(anyhow!("LIBSQL_BOTTOMLESS_AWS_DEFAULT_REGION was not set"))?;
        let access_key_id = self
            .access_key_id
            .clone()
            .ok_or(anyhow!("LIBSQL_BOTTOMLESS_AWS_ACCESS_KEY_ID was not set"))?;
        let secret_access_key = self.secret_access_key.clone().ok_or(anyhow!(
            "LIBSQL_BOTTOMLESS_AWS_SECRET_ACCESS_KEY was not set"
        ))?;
        let conf = aws_sdk_s3::config::Builder::from(&loader.load().await)
            .force_path_style(true)
            .region(Region::new(region))
            .credentials_provider(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "Static",
            ))
            .build();
        Ok(conf)
    }

    pub fn from_env() -> Result<Self> {
        fn env_var(key: &str) -> Result<String> {
            match std::env::var(key) {
                Ok(res) => Ok(res),
                Err(_) => bail!("{} environment variable not set", key),
            }
        }
        fn env_var_or<S: ToString>(key: &str, default_value: S) -> String {
            match std::env::var(key) {
                Ok(res) => res,
                Err(_) => default_value.to_string(),
            }
        }

        let db_id = env_var("LIBSQL_BOTTOMLESS_DATABASE_ID").ok();
        let aws_endpoint = env_var("LIBSQL_BOTTOMLESS_ENDPOINT").ok();
        let bucket_name = env_var_or("LIBSQL_BOTTOMLESS_BUCKET", "bottomless");
        let max_batch_interval = Duration::from_secs(
            env_var_or("LIBSQL_BOTTOMLESS_BATCH_INTERVAL_SECS", 15).parse::<u64>()?,
        );
        let access_key_id = env_var("LIBSQL_BOTTOMLESS_AWS_ACCESS_KEY_ID").ok();
        let secret_access_key = env_var("LIBSQL_BOTTOMLESS_AWS_SECRET_ACCESS_KEY").ok();
        let region = env_var("LIBSQL_BOTTOMLESS_AWS_DEFAULT_REGION").ok();
        let max_frames_per_batch =
            env_var_or("LIBSQL_BOTTOMLESS_BATCH_MAX_FRAMES", 500).parse::<usize>()?;
        let s3_upload_max_parallelism =
            env_var_or("LIBSQL_BOTTOMLESS_S3_PARALLEL_MAX", 32).parse::<usize>()?;
        let restore_transaction_page_swap_after =
            env_var_or("LIBSQL_BOTTOMLESS_RESTORE_TXN_SWAP_THRESHOLD", 1000).parse::<u32>()?;
        let restore_transaction_cache_fpath =
            env_var_or("LIBSQL_BOTTOMLESS_RESTORE_TXN_FILE", ".bottomless.restore");
        let use_compression =
            CompressionKind::parse(&env_var_or("LIBSQL_BOTTOMLESS_COMPRESSION", "gz"))
                .map_err(|e| anyhow!("unknown compression kind: {}", e))?;
        let verify_crc = match env_var_or("LIBSQL_BOTTOMLESS_VERIFY_CRC", true)
            .to_lowercase()
            .as_ref()
        {
            "yes" | "true" | "1" | "y" | "t" => true,
            "no" | "false" | "0" | "n" | "f" => false,
            other => bail!(
                "Invalid LIBSQL_BOTTOMLESS_VERIFY_CRC environment variable: {}",
                other
            ),
        };
        let snapshot_interval = if let Ok(secs) = env_var("LIBSQL_BOTTOMLESS_SNAPSHOT_INTERVAL") {
            Some(Duration::from_secs(secs.parse::<u64>()?))
        } else {
            None
        };
        Ok(Options {
            db_id,
            create_bucket_if_not_exists: true,
            verify_crc,
            use_compression,
            max_batch_interval,
            max_frames_per_batch,
            s3_upload_max_parallelism,
            restore_transaction_page_swap_after,
            aws_endpoint,
            access_key_id,
            secret_access_key,
            region,
            restore_transaction_cache_fpath,
            bucket_name,
            snapshot_interval,
        })
    }
}

impl Replicator {
    pub const UNSET_PAGE_SIZE: usize = usize::MAX;

    pub async fn new<S: Into<String>>(db_path: S) -> Result<Self> {
        Self::with_options(db_path, Options::from_env()?).await
    }

    pub async fn with_options<S: Into<String>>(db_path: S, options: Options) -> Result<Self> {
        let config = options.client_config().await?;
        let client = Client::from_conf(config);
        let bucket = options.bucket_name.clone();
        let generation = Arc::new(ArcSwapOption::default());

        match client.head_bucket().bucket(&bucket).send().await {
            Ok(_) => tracing::info!("Bucket {} exists and is accessible", bucket),
            Err(SdkError::ServiceError(err)) if err.err().is_not_found() => {
                if options.create_bucket_if_not_exists {
                    tracing::info!("Bucket {} not found, recreating", bucket);
                    client.create_bucket().bucket(&bucket).send().await?;
                } else {
                    tracing::error!("Bucket {} does not exist", bucket);
                    return Err(SdkError::ServiceError(err).into());
                }
            }
            Err(e) => {
                tracing::error!("Bucket checking error: {}", e);
                return Err(e.into());
            }
        }

        let db_path = db_path.into();
        let db_name = if let Some(db_id) = options.db_id.clone() {
            db_id
        } else {
            bail!("database id was not set")
        };
        tracing::debug!("Database path: '{}', name: '{}'", db_path, db_name);

        let (flush_trigger, mut flush_trigger_rx) = channel(());
        let (last_committed_frame_no_sender, last_committed_frame_no) = channel(Ok(0));

        let next_frame_no = Arc::new(AtomicU32::new(1));
        let last_sent_frame_no = Arc::new(AtomicU32::new(0));

        let mut _join_set = JoinSet::new();

        let (frames_outbox, mut frames_inbox) = tokio::sync::mpsc::channel(64);
        let _local_backup = {
            let mut copier = WalCopier::new(
                bucket.clone(),
                db_name.clone().into(),
                generation.clone(),
                &db_path,
                options.max_frames_per_batch,
                options.use_compression,
                frames_outbox,
            );
            let next_frame_no = next_frame_no.clone();
            let last_sent_frame_no = last_sent_frame_no.clone();
            let batch_interval = options.max_batch_interval;
            _join_set.spawn(async move {
                loop {
                    let timeout = Instant::now() + batch_interval;
                    let trigger = match timeout_at(timeout, flush_trigger_rx.changed()).await {
                        Ok(Ok(())) => true,
                        Ok(Err(_)) => {
                            return;
                        }
                        Err(_) => {
                            true // timeout reached
                        }
                    };
                    if trigger {
                        let next_frame = next_frame_no.load(Ordering::Acquire);
                        let last_sent_frame =
                            last_sent_frame_no.swap(next_frame - 1, Ordering::Acquire);
                        let frames = (last_sent_frame + 1)..next_frame;

                        if !frames.is_empty() {
                            let res = copier.flush(frames).await;
                            if last_committed_frame_no_sender.send(res).is_err() {
                                // Replicator was probably dropped and therefore corresponding
                                // receiver has been closed
                                return;
                            }
                        }
                    }
                }
            })
        };

        let _s3_upload = {
            let client = client.clone();
            let bucket = options.bucket_name.clone();
            let max_parallelism = options.s3_upload_max_parallelism;
            _join_set.spawn(async move {
                let sem = Arc::new(tokio::sync::Semaphore::new(max_parallelism));
                let mut join_set = JoinSet::new();
                while let Some(fdesc) = frames_inbox.recv().await {
                    tracing::trace!("Received S3 upload request: {}", fdesc);
                    let start = Instant::now();
                    let sem = sem.clone();
                    let permit = sem.acquire_owned().await.unwrap();
                    let client = client.clone();
                    let bucket = bucket.clone();
                    join_set.spawn(async move {
                        let fpath = format!("{}/{}", bucket, fdesc);
                        let body = ByteStream::from_path(&fpath).await.unwrap();
                        if let Err(e) = client
                            .put_object()
                            .bucket(bucket)
                            .key(fdesc)
                            .body(body)
                            .send()
                            .await
                        {
                            tracing::error!("Failed to send {} to S3: {}", fpath, e);
                        } else {
                            tokio::fs::remove_file(&fpath).await.unwrap();
                            let elapsed = Instant::now() - start;
                            tracing::debug!("Uploaded to S3: {} in {:?}", fpath, elapsed);
                        }
                        drop(permit);
                    });
                }
            })
        };
        let (snapshot_notifier, snapshot_waiter) = channel(Ok(None));
        let client = S3Client::new(client, bucket.clone(), db_name.clone());
        Ok(Self {
            client,
            bucket,
            page_size: Self::UNSET_PAGE_SIZE,
            generation,
            next_frame_no,
            last_sent_frame_no,
            flush_trigger,
            last_committed_frame_no,
            verify_crc: options.verify_crc,
            db_path,
            db_name,
            snapshot_waiter,
            snapshot_notifier: Arc::new(snapshot_notifier),
            restore_transaction_page_swap_after: options.restore_transaction_page_swap_after,
            restore_transaction_cache_fpath: options.restore_transaction_cache_fpath.into(),
            use_compression: options.use_compression,
            max_frames_per_batch: options.max_frames_per_batch,
            s3_upload_max_parallelism: options.s3_upload_max_parallelism,
            snapshot_interval: options.snapshot_interval,
            _join_set,
        })
    }

    pub fn next_frame_no(&self) -> u32 {
        self.next_frame_no.load(Ordering::Acquire)
    }

    pub fn last_known_frame(&self) -> u32 {
        self.next_frame_no() - 1
    }

    pub fn last_sent_frame_no(&self) -> u32 {
        self.last_sent_frame_no.load(Ordering::Acquire)
    }

    pub async fn wait_until_snapshotted(&mut self) -> Result<bool> {
        if let Ok(generation) = self.generation() {
            if !self.db_file_has_data().await {
                tracing::debug!("Not snapshotting, the main db file does not exist or is empty");
                let _ = self.snapshot_notifier.send(Ok(Some(generation)));
                return Ok(false);
            }
            tracing::debug!("waiting for generation snapshot {} to complete", generation);
            let res = self
                .snapshot_waiter
                .wait_for(|result| match result {
                    Ok(Some(gen)) => *gen == generation,
                    Ok(None) => false,
                    Err(_) => true,
                })
                .await?;
            match res.deref() {
                Ok(_) => Ok(true),
                Err(e) => Err(anyhow!("Failed snapshot generation {}: {}", generation, e)),
            }
        } else {
            Ok(false)
        }
    }

    /// Waits until the commit for a given frame_no or higher was given.
    pub async fn wait_until_committed(&mut self, frame_no: u32) -> Result<u32> {
        let res = self
            .last_committed_frame_no
            .wait_for(|result| match result {
                Ok(last_committed) => *last_committed >= frame_no,
                Err(_) => true,
            })
            .await?;

        match res.deref() {
            Ok(last_committed) => {
                tracing::trace!(
                    "Confirmed commit of frame no. {} (waited for >= {})",
                    last_committed,
                    frame_no
                );
                Ok(*last_committed)
            }
            Err(e) => Err(anyhow!("Failed to flush frames: {}", e)),
        }
    }

    /// Returns number of frames waiting to be replicated.
    pub fn pending_frames(&self) -> u32 {
        self.next_frame_no() - self.last_sent_frame_no() - 1
    }

    // The database can use different page size - as soon as it's known,
    // it should be communicated to the replicator via this call.
    // NOTICE: in practice, WAL journaling mode does not allow changing page sizes,
    // so verifying that it hasn't changed is a panic check. Perhaps in the future
    // it will be useful, if WAL ever allows changing the page size.
    pub fn set_page_size(&mut self, page_size: usize) -> Result<()> {
        if self.page_size != page_size {
            tracing::trace!("Setting page size to: {}", page_size);
        }
        if self.page_size != Self::UNSET_PAGE_SIZE && self.page_size != page_size {
            return Err(anyhow::anyhow!(
                "Cannot set page size to {}, it was already set to {}",
                page_size,
                self.page_size
            ));
        }
        self.page_size = page_size;
        Ok(())
    }

    fn reset_frames(&mut self, frame_no: u32) {
        let last_sent = self.last_sent_frame_no();
        self.next_frame_no.store(frame_no + 1, Ordering::Release);
        self.last_sent_frame_no
            .store(last_sent.min(frame_no), Ordering::Release);
    }

    // Starts a new generation for this replicator instance
    pub fn new_generation(&mut self) -> Option<Uuid> {
        let curr = Uuid::new_v7();
        let prev = self.set_generation(curr);
        if let Some(prev) = prev {
            if prev != curr {
                // try to store dependency between previous and current generation
                tracing::trace!("New generation {} (parent: {})", curr, prev);
                let client = self.client.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.store_dependency(&prev, &curr).await {
                        tracing::error!(
                            "Failed to store dependency between parent {} and child {} generations: {}",
                            prev,
                            curr,
                            e
                        );
                    }
                });
            }
        }
        prev
    }

    // Sets a generation for this replicator instance. This function
    // should be called if a generation number from S3-compatible storage
    // is reused in this session.
    pub fn set_generation(&mut self, generation: Uuid) -> Option<Uuid> {
        let prev_generation = self.generation.swap(Some(Arc::new(generation)));
        self.reset_frames(0);
        if let Some(prev) = prev_generation.as_deref() {
            tracing::debug!("Generation changed from {} -> {}", prev, generation);
            Some(*prev)
        } else {
            tracing::debug!("Generation set {}", generation);
            None
        }
    }

    pub fn generation(&self) -> Result<Uuid> {
        let guard = self.generation.load();
        guard
            .as_deref()
            .cloned()
            .ok_or(anyhow!("Replicator generation was not initialized"))
    }

    // Returns the current last valid frame in the replicated log
    pub fn peek_last_valid_frame(&self) -> u32 {
        self.next_frame_no().saturating_sub(1)
    }

    // Sets the last valid frame in the replicated log.
    pub fn register_last_valid_frame(&mut self, frame: u32) {
        let last_valid_frame = self.peek_last_valid_frame();
        if frame != last_valid_frame {
            if last_valid_frame != 0 {
                tracing::error!(
                    "[BUG] Local max valid frame is {}, while replicator thinks it's {}",
                    frame,
                    last_valid_frame
                );
            }
            self.reset_frames(frame);
        }
    }

    /// Submit next `frame_count` of frames to be replicated.
    pub fn submit_frames(&mut self, frame_count: u32) {
        let prev = self.next_frame_no.fetch_add(frame_count, Ordering::SeqCst);
        let last_sent = self.last_sent_frame_no();
        let most_recent = prev + frame_count - 1;
        if most_recent - last_sent >= self.max_frames_per_batch as u32 {
            self.request_flush();
        }
    }

    pub fn request_flush(&self) {
        tracing::trace!("Requesting flush");
        let _ = self.flush_trigger.send(());
    }

    // Drops uncommitted frames newer than given last valid frame
    pub fn rollback_to_frame(&mut self, last_valid_frame: u32) {
        // NOTICE: O(size), can be optimized to O(removed) if ever needed
        self.reset_frames(last_valid_frame);
        tracing::debug!("Rolled back to {}", last_valid_frame);
    }

    // Tries to read the local change counter from the given database file
    async fn read_change_counter(reader: &mut File) -> Result<[u8; 4]> {
        let mut counter = [0u8; 4];
        reader.seek(std::io::SeekFrom::Start(24)).await?;
        reader.read_exact(&mut counter).await?;
        Ok(counter)
    }

    // Tries to read the local page size from the given database file
    async fn read_page_size(reader: &mut File) -> Result<usize> {
        reader.seek(SeekFrom::Start(16)).await?;
        let page_size = reader.read_u16().await?;
        if page_size == 1 {
            Ok(65536)
        } else {
            Ok(page_size as usize)
        }
    }

    // Returns the compressed database file path and its change counter, extracted
    // from the header of page1 at offset 24..27 (as per SQLite documentation).
    pub async fn maybe_compress_main_db_file(
        mut reader: File,
        compression: CompressionKind,
    ) -> Result<ByteStream> {
        reader.seek(SeekFrom::Start(0)).await?;
        match compression {
            CompressionKind::None => Ok(ByteStream::read_from().file(reader).build().await?),
            CompressionKind::Gzip => {
                let compressed_file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .read(true)
                    .truncate(true)
                    .open("db.gz")
                    .await?;
                let mut writer = GzipEncoder::new(compressed_file);
                let size = tokio::io::copy(&mut reader, &mut writer).await?;
                tracing::trace!("Compressed database file ({} bytes) into db.gz", size);
                writer.shutdown().await?;
                Ok(ByteStream::from_path("db.gz").await?)
            }
        }
    }
    // Replicates local WAL pages to S3, if local WAL is present.
    // This function is called under the assumption that if local WAL
    // file is present, it was already detected to be newer than its
    // remote counterpart.
    pub async fn maybe_replicate_wal(&mut self) -> Result<()> {
        let generation = self.generation()?;
        let wal = match WalFileReader::open(&format!("{}-wal", &self.db_path)).await {
            Ok(Some(file)) => file,
            _ => {
                tracing::info!("Local WAL not present - not replicating");
                return Ok(());
            }
        };

        self.client
            .store_metadata(&generation, wal.page_size(), wal.checksum())
            .await?;

        let frame_count = wal.frame_count().await;
        tracing::trace!("Local WAL pages: {}", frame_count);
        self.submit_frames(frame_count);
        self.request_flush();
        let last_written_frame = self.wait_until_committed(frame_count - 1).await?;
        tracing::info!("Backed up WAL frames up to {}", last_written_frame);
        let pending_frames = self.pending_frames();
        if pending_frames != 0 {
            tracing::warn!(
                "Uncommitted WAL entries: {} frames in total",
                pending_frames
            );
        }
        tracing::info!("Local WAL replicated");
        Ok(())
    }

    /// Check if the local database file exists and contains data.
    async fn db_file_has_data(&self) -> bool {
        let file = match File::open(&self.db_path).await {
            Ok(file) => file,
            Err(_) => return false,
        };
        match file.metadata().await {
            Ok(metadata) => metadata.len() > 0,
            Err(_) => false,
        }
    }

    /// Returns info, which generation had the most recent snapshot.
    async fn get_last_snapshot(&self) -> Option<Uuid> {
        let snapshot_lock_file_path = format!("{}.last-snapshot", self.db_path);
        let mut f = File::open(snapshot_lock_file_path).await.ok()?;
        let mut buf = [0u8; 16];
        f.read_exact(&mut buf).await.ok()?;
        Some(Uuid::from_bytes(buf))
    }

    async fn save_last_snapshot(db_path: &str, generation: &Uuid) -> Result<()> {
        let snapshot_lock_file_path = format!("{}.last-snapshot", db_path);
        let mut f = OpenOptions::new()
            .truncate(true)
            .write(true)
            .create(true)
            .open(snapshot_lock_file_path)
            .await?;
        f.write_all(generation.as_ref()).await?;
        f.shutdown().await?;
        tracing::trace!(
            "cached last snapshotted generation: {} ({:?})",
            generation,
            generation.date_time().unwrap()
        );
        Ok(())
    }

    pub fn skip_snapshot_for_current_generation(&self) {
        let generation = self.generation.load().as_deref().cloned();
        let _ = self.snapshot_notifier.send(Ok(generation));
    }

    async fn snapshot_interval_passed(&self) -> bool {
        if let Some(snapshot_interval) = self.snapshot_interval {
            if let Some(snapshot_gen) = self.get_last_snapshot().await {
                if let Some(snapshot_time) = snapshot_gen.date_time() {
                    let next_snapshot_date = snapshot_time + snapshot_interval;
                    tracing::trace!(
                        "Last known snapshot: {} ({:?}) - next one after {}",
                        snapshot_gen,
                        snapshot_time,
                        next_snapshot_date
                    );
                    let now = Utc::now().naive_utc();
                    if next_snapshot_date > now {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Tries to create a snapshot of a main database file - if it exists and is actual - and
    /// uploads it to S3.
    ///
    /// If snapshot process was started, an awaiter will be returned - it can be used to wait for
    /// snapshot completion.
    pub async fn snapshot(&mut self) -> Result<Option<JoinHandle<()>>> {
        let generation = self.generation()?;
        if !self.db_file_has_data().await {
            tracing::debug!("Not snapshotting, the main db file does not exist or is empty");
            let _ = self.snapshot_notifier.send(Ok(Some(generation)));
            return Ok(None);
        }
        if !self.snapshot_interval_passed().await {
            tracing::trace!("Not snapshotting, snapshot interval is still in progress");
            let _ = self.snapshot_notifier.send(Ok(Some(generation)));
            return Ok(None);
        }
        tracing::debug!("Snapshotting generation {}", generation);
        let start_ts = Instant::now();

        let mut db_file = File::open(&self.db_path).await?;
        let change_counter = Self::read_change_counter(&mut db_file).await?;

        let snapshot_notifier = self.snapshot_notifier.clone();
        let compression = self.use_compression;
        let db_path = self.db_path.clone();
        let client = self.client.clone();
        let handle = tokio::spawn(async move {
            tracing::trace!("Start snapshotting generation {}", generation);
            let start = Instant::now();
            let body = match Self::maybe_compress_main_db_file(db_file, compression).await {
                Ok(file) => file,
                Err(e) => {
                    tracing::error!(
                        "Failed to compress db file (generation {}): {}",
                        generation,
                        e
                    );
                    let _ = snapshot_notifier.send(Err(e));
                    return;
                }
            };
            let mut result = client.store_snapshot(&generation, compression, body).await;
            if let Err(e) = result {
                tracing::error!(
                    "Failed to upload snapshot for generation {}: {:?}",
                    generation,
                    e
                );
                let _ = snapshot_notifier.send(Err(e.into()));
                return;
            }
            result = client
                .store_change_counter(&generation, change_counter)
                .await;
            if let Err(e) = result {
                tracing::error!(
                    "Failed to upload change counter for generation {}: {:?}",
                    generation,
                    e
                );
                let _ = snapshot_notifier.send(Err(e.into()));
                return;
            }
            if let Err(e) = Self::save_last_snapshot(&db_path, &generation).await {
                tracing::error!(
                    "failed to save the latest known snapshot {}: {}",
                    generation,
                    e
                );
            }
            let _ = snapshot_notifier.send(Ok(Some(generation)));
            let elapsed = Instant::now() - start;
            tracing::debug!("Snapshot upload finished (took {:?})", elapsed);
            let _ = tokio::fs::remove_file(format!("db.{}", compression)).await;
        });
        let elapsed = Instant::now() - start_ts;
        tracing::debug!("Scheduled DB snapshot {} (took {:?})", generation, elapsed);

        Ok(Some(handle))
    }

    // Returns the number of pages stored in the local WAL file, or 0, if there aren't any.
    async fn get_local_wal_page_count(&mut self) -> u32 {
        match WalFileReader::open(&format!("{}-wal", &self.db_path)).await {
            Ok(None) => 0,
            Ok(Some(wal)) => {
                let page_size = wal.page_size();
                if self.set_page_size(page_size as usize).is_err() {
                    return 0;
                }
                wal.frame_count().await
            }
            Err(_) => 0,
        }
    }

    /// Restores the database state from given remote generation
    /// On success, returns the RestoreAction, and whether the database was recovered from backup.
    async fn restore_from(
        &mut self,
        generation: Uuid,
        timestamp: Option<NaiveDateTime>,
    ) -> Result<(RestoreAction, bool)> {
        if let Some(tombstone) = self.client.get_tombstone().await? {
            if let Some(timestamp) = generation.date_time() {
                if tombstone >= timestamp {
                    bail!(
                        "Couldn't restore from generation {}. Database '{}' has been tombstoned at {}.",
                        generation,
                        self.db_name,
                        tombstone
                    );
                }
            }
        }

        let start_ts = Instant::now();
        // first check if there are any remaining files that we didn't manage to upload
        // on time in the last run
        self.upload_remaining_files(&generation).await?;

        let last_frame = self
            .client
            .get_last_wal_segment(&generation)
            .await?
            .map(|w| w.last_frame_no)
            .unwrap_or(0);
        tracing::debug!(
            "Last consistent remote frame in generation {}: {}.",
            generation,
            last_frame
        );
        if let Some(action) = self.compare_with_local(generation, last_frame).await? {
            return Ok((action, false));
        }

        // at this point we know, we should do a full restore

        let backup_path = format!("{}.bottomless.backup", self.db_path);
        tokio::fs::rename(&self.db_path, &backup_path).await.ok(); // Best effort
        match self.full_restore(generation, timestamp, last_frame).await {
            Ok(result) => {
                let elapsed = Instant::now() - start_ts;
                tracing::info!("Finished database restoration in {:?}", elapsed);
                tokio::fs::remove_file(backup_path).await.ok();
                Ok(result)
            }
            Err(e) => {
                tracing::error!("failed to restore the database: {}. Rollback", e);
                tokio::fs::rename(&backup_path, &self.db_path).await.ok();
                Err(e)
            }
        }
    }

    async fn full_restore(
        &mut self,
        generation: Uuid,
        timestamp: Option<NaiveDateTime>,
        last_frame: u32,
    ) -> Result<(RestoreAction, bool)> {
        let _ = self.remove_wal_files().await; // best effort, WAL files may not exists
        let mut db = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.db_path)
            .await?;

        let mut restore_stack = Vec::new();

        // If the db file is not present, the database could have been empty
        let mut current = Some(generation);
        while let Some(curr) = current.take() {
            // stash current generation - we'll use it to replay WAL across generations since the
            // last snapshot
            restore_stack.push(curr);
            let restored = self.restore_from_snapshot(&curr, &mut db).await?;
            if restored {
                Self::save_last_snapshot(&self.db_path, &curr).await?;
                break;
            } else {
                if restore_stack.len() > MAX_RESTORE_STACK_DEPTH {
                    bail!("Restoration failed: maximum number of generations to restore from was reached.");
                }
                tracing::debug!("No snapshot found on the generation {}", curr);
                // there was no snapshot to restore from, it means that we either:
                // 1. Have only WAL to restore from - case when we're at the initial generation
                //    of the database.
                // 2. Snapshot never existed - in that case try to reach for parent generation
                //    of the current one and read snapshot from there.
                current = self.client.get_dependency(&curr).await?;
                if let Some(prev) = &current {
                    tracing::debug!("Rolling restore back from generation {} to {}", curr, prev);
                }
            }
        }

        tracing::trace!(
            "Restoring database from {} generations",
            restore_stack.len()
        );

        let mut applied_wal_frame = false;
        while let Some(gen) = restore_stack.pop() {
            if let Some((page_size, checksum)) = self.client.get_metadata(&gen).await? {
                self.set_page_size(page_size as usize)?;
                let last_frame = if restore_stack.is_empty() {
                    // we're at the last generation to restore from, it may still being written to
                    // so we constraint the restore to a frame checked at the beginning of the
                    // restore procedure
                    Some(last_frame)
                } else {
                    None
                };
                if let Some(ts) = self
                    .restore_wal(
                        &gen,
                        page_size as usize,
                        last_frame,
                        checksum,
                        timestamp,
                        &mut db,
                    )
                    .await?
                {
                    tracing::debug!(
                        "restored WAL frames in generation {} up to timestamp {}",
                        generation,
                        ts
                    );
                    applied_wal_frame = true;
                }
            } else {
                tracing::info!(".meta object not found, skipping WAL restore.");
            };
        }

        db.shutdown().await?;

        if applied_wal_frame {
            tracing::info!("WAL file has been applied onto database file in generation {}. Requesting snapshot.", generation);
            Ok::<_, anyhow::Error>((RestoreAction::SnapshotMainDbFile, true))
        } else {
            tracing::info!("Reusing generation {}.", generation);
            // since WAL was not applied, we can reuse the latest generation
            Ok::<_, anyhow::Error>((RestoreAction::ReuseGeneration(generation), true))
        }
    }

    /// Compares S3 generation backup state against current local database file to determine
    /// if we are up to date (returned restore action) or should we perform restoration.
    async fn compare_with_local(
        &mut self,
        generation: Uuid,
        last_consistent_frame: u32,
    ) -> Result<Option<RestoreAction>> {
        // Check if the database needs to be restored by inspecting the database
        // change counter and the WAL size.
        let local_counter = match File::open(&self.db_path).await {
            Ok(mut db) => {
                // While reading the main database file for the first time,
                // page size from an existing database should be set.
                if let Ok(page_size) = Self::read_page_size(&mut db).await {
                    self.set_page_size(page_size)?;
                }
                Self::read_change_counter(&mut db).await.unwrap_or([0u8; 4])
            }
            Err(_) => [0u8; 4],
        };

        let remote_counter = self.client.get_change_counter(&generation).await?;
        tracing::debug!("Counters: l={:?}, r={:?}", local_counter, remote_counter);

        let wal_pages = self.get_local_wal_page_count().await;
        // We impersonate as a given generation, since we're comparing against local backup at that
        // generation. This is used later in [Self::new_generation] to create a dependency between
        // this generation and a new one.
        self.generation.store(Some(Arc::new(generation)));
        match local_counter.cmp(&remote_counter) {
            std::cmp::Ordering::Equal => {
                tracing::debug!(
                    "Consistent: {}; wal pages: {}",
                    last_consistent_frame,
                    wal_pages
                );
                match wal_pages.cmp(&last_consistent_frame) {
                    std::cmp::Ordering::Equal => {
                        tracing::info!(
                            "Remote generation is up-to-date, reusing it in this session"
                        );
                        self.reset_frames(wal_pages + 1);
                        Ok(Some(RestoreAction::ReuseGeneration(generation)))
                    }
                    std::cmp::Ordering::Greater => {
                        tracing::info!("Local change counter matches the remote one, but local WAL contains newer data from generation {}, which needs to be replicated.", generation);
                        Ok(Some(RestoreAction::SnapshotMainDbFile))
                    }
                    std::cmp::Ordering::Less => Ok(None),
                }
            }
            std::cmp::Ordering::Greater => {
                tracing::info!("Local change counter is larger than its remote counterpart - a new snapshot needs to be replicated (generation: {})", generation);
                Ok(Some(RestoreAction::SnapshotMainDbFile))
            }
            std::cmp::Ordering::Less => Ok(None),
        }
    }

    async fn restore_from_snapshot(&mut self, generation: &Uuid, db: &mut File) -> Result<bool> {
        let main_db_path = match self.use_compression {
            CompressionKind::None => "db.db",
            CompressionKind::Gzip => "db.gz",
        };

        if let Ok(Some(db_file)) = self.client.try_get(generation, main_db_path).await {
            let mut body_reader = db_file.into_async_read();
            let db_size = match self.use_compression {
                CompressionKind::None => tokio::io::copy(&mut body_reader, db).await?,
                CompressionKind::Gzip => {
                    let mut decompress_reader = async_compression::tokio::bufread::GzipDecoder::new(
                        tokio::io::BufReader::new(body_reader),
                    );
                    tokio::io::copy(&mut decompress_reader, db).await?
                }
            };
            db.flush().await?;

            let page_size = Self::read_page_size(db).await?;
            self.set_page_size(page_size)?;
            tracing::info!("Restored the main database file ({} bytes)", db_size);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn restore_wal(
        &self,
        generation: &Uuid,
        page_size: usize,
        last_consistent_frame: Option<u32>,
        mut checksum: u64,
        utc_time: Option<NaiveDateTime>,
        db: &mut File,
    ) -> Result<Option<NaiveDateTime>> {
        let mut page_buf = {
            let mut v = Vec::with_capacity(page_size);
            v.spare_capacity_mut();
            unsafe { v.set_len(page_size) };
            v
        };
        let mut last_applied_timestamp = None;
        let mut pending_pages = TransactionPageCache::new(
            self.restore_transaction_page_swap_after,
            page_size as u32,
            self.restore_transaction_cache_fpath.clone(),
        );
        let mut last_received_frame_no = 0;
        let mut keys = self.client.list_generation_keys(generation);
        while let Some(key) = keys.next().await? {
            tracing::debug!("Loading {}", key);

            let summary = match WalSegmentSummary::parse(&key) {
                Some(result) => result,
                None => {
                    if !key.ends_with(".gz")
                        && !key.ends_with(".db")
                        && !key.ends_with(".meta")
                        && !key.ends_with(".dep")
                        && !key.ends_with(".changecounter")
                    {
                        tracing::warn!("Failed to parse frame/page from key {}", key);
                    }
                    continue;
                }
            };
            if summary.first_frame_no != last_received_frame_no + 1 {
                tracing::warn!("Missing series of consecutive frames. Last applied frame: {}, next found: {}. Stopping the restoration process",
                            last_received_frame_no, summary.first_frame_no);
                break;
            }
            if let Some(frame) = last_consistent_frame {
                if summary.last_frame_no > frame {
                    tracing::warn!("Remote log contains frame {} larger than last consistent frame ({}), stopping the restoration process",
                                summary.last_frame_no, frame);
                    break;
                }
            }
            if let Some(threshold) = utc_time {
                if summary.timestamp > threshold {
                    tracing::info!("Frame batch {} has timestamp more recent than expected {}. Stopping recovery.", key, summary.timestamp);
                    break; // reached end of restoration timestamp
                }
            }
            let frame = self.client.get_object(key).await?;
            let mut frameno = summary.first_frame_no;
            let mut reader =
                BatchReader::new(frameno, frame, self.page_size, summary.compression_kind);

            while let Some(frame) = reader.next_frame_header().await? {
                let pgno = frame.pgno();
                let page_size = self.page_size;
                reader.next_page(&mut page_buf).await?;
                if self.verify_crc {
                    checksum = frame.verify(checksum, &page_buf)?;
                }
                pending_pages.insert(pgno, &page_buf).await?;
                if frame.is_committed() {
                    let pending_pages = std::mem::replace(
                        &mut pending_pages,
                        TransactionPageCache::new(
                            self.restore_transaction_page_swap_after,
                            page_size as u32,
                            self.restore_transaction_cache_fpath.clone(),
                        ),
                    );
                    pending_pages.flush(db).await?;
                    last_applied_timestamp = Some(summary.timestamp);
                }
                frameno += 1;
                last_received_frame_no += 1;
            }
            db.flush().await?;
        }
        Ok(last_applied_timestamp)
    }

    async fn remove_wal_files(&self) -> Result<()> {
        tracing::debug!("Overwriting any existing WAL file: {}-wal", &self.db_path);
        tokio::fs::remove_file(&format!("{}-wal", &self.db_path)).await?;
        tokio::fs::remove_file(&format!("{}-shm", &self.db_path)).await?;
        Ok(())
    }

    /// Restores the database state from newest remote generation
    /// On success, returns the RestoreAction, and whether the database was recovered from backup.
    pub async fn restore(
        &mut self,
        generation: Option<Uuid>,
        timestamp: Option<NaiveDateTime>,
    ) -> Result<(RestoreAction, bool)> {
        let generation = match generation {
            Some(gen) => gen,
            None => match self
                .client
                .latest_generation_before(timestamp.as_ref())
                .await
            {
                Some(gen) => gen,
                None => {
                    tracing::debug!("No generation found, nothing to restore");
                    return Ok((RestoreAction::SnapshotMainDbFile, false));
                }
            },
        };

        tracing::info!(
            "Restoring from generation {} ({:?})",
            generation,
            generation.date_time().unwrap()
        );
        self.restore_from(generation, timestamp).await
    }

    async fn upload_remaining_files(&self, generation: &Uuid) -> Result<()> {
        let prefix = format!("{}-{}", self.db_name, generation);
        let dir = format!("{}/{}-{}", self.bucket, self.db_name, generation);
        if tokio::fs::try_exists(&dir).await? {
            let mut files = tokio::fs::read_dir(&dir).await?;
            let sem = Arc::new(tokio::sync::Semaphore::new(self.s3_upload_max_parallelism));
            while let Some(file) = files.next_entry().await? {
                let fpath = file.path();
                if let Some(key) = Self::fpath_to_key(&fpath, &prefix) {
                    tracing::trace!("Requesting upload of the remaining backup file: {}", key);
                    let permit = sem.clone().acquire_owned().await?;
                    let key = key.to_string();
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        let body = ByteStream::from_path(&fpath).await.unwrap();
                        if let Err(e) = client.put_object(&key, body).await {
                            tracing::error!("Failed to send {} to S3: {}", key, e);
                        } else {
                            tokio::fs::remove_file(&fpath).await.unwrap();
                            tracing::trace!("Uploaded to S3: {}", key);
                        }
                        drop(permit);
                    });
                }
            }
            // wait for all started upload tasks to finish
            let _ = sem
                .acquire_many(self.s3_upload_max_parallelism as u32)
                .await?;
            if let Err(e) = tokio::fs::remove_dir(&dir).await {
                tracing::warn!("Couldn't remove backed up directory {}: {}", dir, e);
            }
        }
        Ok(())
    }

    fn fpath_to_key<'a>(fpath: &'a Path, dir: &str) -> Option<&'a str> {
        let str = fpath.to_str()?;
        if str.ends_with(".db")
            | str.ends_with(".gz")
            | str.ends_with(".raw")
            | str.ends_with(".meta")
            | str.ends_with(".dep")
            | str.ends_with(".changecounter")
        {
            let idx = str.rfind(dir)?;
            return Some(&str[idx..]);
        }
        None
    }
}

pub struct Context {
    pub replicator: Replicator,
    pub runtime: tokio::runtime::Runtime,
}

#[derive(Debug, Clone, Copy, Default, Ord, PartialOrd, Eq, PartialEq)]
pub enum CompressionKind {
    #[default]
    None,
    Gzip,
}

impl CompressionKind {
    pub fn parse(kind: &str) -> std::result::Result<Self, &str> {
        match kind {
            "gz" | "gzip" => Ok(CompressionKind::Gzip),
            "raw" | "" => Ok(CompressionKind::None),
            other => Err(other),
        }
    }
}

impl std::fmt::Display for CompressionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionKind::None => write!(f, "raw"),
            CompressionKind::Gzip => write!(f, "gz"),
        }
    }
}
