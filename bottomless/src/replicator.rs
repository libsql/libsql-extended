use crate::backup::WalCopier;
use crate::read::BatchReader;
use crate::transaction_cache::TransactionPageCache;
use crate::uuid_utils::decode_unix_timestamp;
use crate::wal::WalFileReader;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder;
use aws_sdk_s3::operation::get_object::GetObjectError;
use aws_sdk_s3::operation::list_objects::builders::ListObjectsFluentBuilder;
use aws_sdk_s3::operation::list_objects::ListObjectsOutput;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::{Client, Config};
use bytes::{Buf, Bytes, BytesMut};
use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use std::io::SeekFrom;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch::{channel, Receiver, Sender};
use tokio::task::JoinSet;
use tokio::time::{timeout_at, Instant};
use uuid::{NoContext, Uuid};

pub type Result<T> = anyhow::Result<T>;

#[derive(Debug)]
pub struct Replicator {
    pub client: Client,

    /// Frame number, incremented whenever a new frame is written from SQLite.
    next_frame_no: Arc<AtomicU32>,
    /// Last frame which has been requested to be sent to S3.
    /// Always: [last_sent_frame_no] <= [next_frame_no].
    last_sent_frame_no: Arc<AtomicU32>,
    /// Last frame which has been confirmed as stored locally outside of WAL file.
    /// Always: [last_committed_frame_no] <= [last_sent_frame_no].
    last_committed_frame_no: Receiver<Result<u32>>,
    flush_trigger: Sender<()>,

    pub page_size: usize,
    restore_transaction_page_swap_after: u32,
    restore_transaction_cache_fpath: Arc<str>,
    generation: Arc<ArcSwap<Uuid>>,
    pub commits_in_current_generation: Arc<AtomicU32>,
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

#[derive(Debug)]
pub enum RestoreAction {
    None,
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
}

impl Options {
    pub async fn client_config(&self) -> Config {
        let mut loader = aws_config::from_env();
        if let Some(endpoint) = self.aws_endpoint.as_deref() {
            loader = loader.endpoint_url(endpoint);
        }
        aws_sdk_s3::config::Builder::from(&loader.load().await)
            .force_path_style(true)
            .build()
    }

    pub fn from_env() -> Result<Self> {
        let mut options = Self::default();
        if let Ok(key) = std::env::var("LIBSQL_BOTTOMLESS_ENDPOINT") {
            options.aws_endpoint = Some(key);
        }
        if let Ok(bucket_name) = std::env::var("LIBSQL_BOTTOMLESS_BUCKET") {
            options.bucket_name = bucket_name;
        }
        if let Ok(seconds) = std::env::var("LIBSQL_BOTTOMLESS_BATCH_INTERVAL_SECS") {
            if let Ok(seconds) = seconds.parse::<u64>() {
                options.max_batch_interval = Duration::from_secs(seconds);
            }
        }
        if let Ok(count) = std::env::var("LIBSQL_BOTTOMLESS_BATCH_MAX_FRAMES") {
            match count.parse::<usize>() {
                Ok(count) => options.max_frames_per_batch = count,
                Err(e) => {
                    return Err(anyhow!(
                        "Invalid LIBSQL_BOTTOMLESS_BATCH_MAX_FRAMES environment variable: {}",
                        e
                    ))
                }
            }
        }
        if let Ok(parallelism) = std::env::var("LIBSQL_BOTTOMLESS_S3_PARALLEL_MAX") {
            match parallelism.parse::<usize>() {
                Ok(parallelism) => options.s3_upload_max_parallelism = parallelism,
                Err(e) => {
                    return Err(anyhow!(
                        "Invalid LIBSQL_BOTTOMLESS_S3_PARALLEL_MAX environment variable: {}",
                        e
                    ))
                }
            }
        }
        if let Ok(swap_after) = std::env::var("LIBSQL_BOTTOMLESS_RESTORE_TXN_SWAP_THRESHOLD") {
            match swap_after.parse::<u32>() {
                Ok(swap_after) => options.restore_transaction_page_swap_after = swap_after,
                Err(e) => {
                    return Err(anyhow!(
                    "Invalid LIBSQL_BOTTOMLESS_RESTORE_TXN_SWAP_THRESHOLD environment variable: {}",
                    e
                ))
                }
            }
        }
        if let Ok(fpath) = std::env::var("LIBSQL_BOTTOMLESS_RESTORE_TXN_FILE") {
            options.restore_transaction_cache_fpath = fpath;
        }
        if let Ok(compression) = std::env::var("LIBSQL_BOTTOMLESS_COMPRESSION") {
            match CompressionKind::parse(&compression) {
                Ok(compression) => options.use_compression = compression,
                Err(e) => {
                    return Err(anyhow!(
                        "Invalid LIBSQL_BOTTOMLESS_COMPRESSION environment variable: {}",
                        e
                    ))
                }
            }
        }
        if let Ok(verify) = std::env::var("LIBSQL_BOTTOMLESS_VERIFY_CRC") {
            match verify.to_lowercase().as_ref() {
                "yes" | "true" | "1" | "y" | "t" => options.verify_crc = true,
                "no" | "false" | "0" | "n" | "f" => options.verify_crc = false,
                other => {
                    return Err(anyhow!(
                        "Invalid LIBSQL_BOTTOMLESS_VERIFY_CRC environment variable: {}",
                        other
                    ))
                }
            }
        }
        Ok(options)
    }
}

impl Default for Options {
    fn default() -> Self {
        let db_id = std::env::var("LIBSQL_BOTTOMLESS_DATABASE_ID").ok();
        Options {
            create_bucket_if_not_exists: true,
            verify_crc: true,
            use_compression: CompressionKind::Gzip,
            max_batch_interval: Duration::from_secs(15),
            max_frames_per_batch: 500, // basically half of the default SQLite checkpoint size
            s3_upload_max_parallelism: 32,
            restore_transaction_page_swap_after: 1000,
            db_id,
            aws_endpoint: None,
            restore_transaction_cache_fpath: ".bottomless.restore".to_string(),
            bucket_name: "bottomless".to_string(),
        }
    }
}

impl Replicator {
    pub const UNSET_PAGE_SIZE: usize = usize::MAX;

    pub async fn new<S: Into<String>>(db_path: S) -> Result<Self> {
        Self::with_options(db_path, Options::from_env()?).await
    }

    pub async fn with_options<S: Into<String>>(db_path: S, options: Options) -> Result<Self> {
        tracing::trace!(
            "Starting bottomless replicator with options: {:#?}",
            options
        );
        let config = options.client_config().await;
        let client = Client::from_conf(config);
        let bucket = options.bucket_name.clone();
        let generation = Arc::new(ArcSwap::new(Arc::new(Self::generate_generation())));
        tracing::debug!("Generation {}", generation.load());

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
        let db_name = {
            let db_id = options.db_id.unwrap_or_default();
            let name = match db_path.find('/') {
                Some(index) => &db_path[..index],
                None => &db_path,
            };
            db_id + ":" + name
        };
        tracing::debug!("Database path: '{}', name: '{}'", db_path, db_name);

        let (flush_trigger, mut flush_trigger_rx) = channel(());
        let (last_committed_frame_no_sender, last_committed_frame_no) = channel(Ok(0));

        let next_frame_no = Arc::new(AtomicU32::new(1));
        let last_sent_frame_no = Arc::new(AtomicU32::new(0));
        let commits_in_current_generation = Arc::new(AtomicU32::new(0));

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
        Ok(Self {
            client,
            bucket,
            page_size: Self::UNSET_PAGE_SIZE,
            generation,
            commits_in_current_generation,
            next_frame_no,
            last_sent_frame_no,
            flush_trigger,
            last_committed_frame_no,
            verify_crc: options.verify_crc,
            db_path,
            db_name,
            restore_transaction_page_swap_after: options.restore_transaction_page_swap_after,
            restore_transaction_cache_fpath: options.restore_transaction_cache_fpath.into(),
            use_compression: options.use_compression,
            max_frames_per_batch: options.max_frames_per_batch,
            s3_upload_max_parallelism: options.s3_upload_max_parallelism,
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

    pub fn commits_in_current_generation(&self) -> u32 {
        self.commits_in_current_generation.load(Ordering::Acquire)
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

    // Gets an object from the current bucket
    fn get_object(&self, key: String) -> GetObjectFluentBuilder {
        self.client.get_object().bucket(&self.bucket).key(key)
    }

    // Lists objects from the current bucket
    fn list_objects(&self) -> ListObjectsFluentBuilder {
        self.client.list_objects().bucket(&self.bucket)
    }

    fn reset_frames(&mut self, frame_no: u32) {
        let last_sent = self.last_sent_frame_no();
        self.next_frame_no.store(frame_no + 1, Ordering::Release);
        self.last_sent_frame_no
            .store(last_sent.min(frame_no), Ordering::Release);
    }

    // Generates a new generation UUID v7, which contains a timestamp and is binary-sortable.
    // This timestamp goes back in time - that allows us to list newest generations
    // first in the S3-compatible bucket, under the assumption that fetching newest generations
    // is the most common operation.
    // NOTICE: at the time of writing, uuid v7 is an unstable feature of the uuid crate
    fn generate_generation() -> Uuid {
        let ts = uuid::timestamp::Timestamp::now(uuid::NoContext);
        Self::generation_from_timestamp(ts)
    }

    fn generation_from_timestamp(ts: uuid::Timestamp) -> Uuid {
        let (seconds, nanos) = ts.to_unix();
        let (seconds, nanos) = (253370761200 - seconds, 999999999 - nanos);
        let synthetic_ts = uuid::Timestamp::from_unix(uuid::NoContext, seconds, nanos);
        crate::uuid_utils::new_v7(synthetic_ts)
    }

    pub fn generation_to_timestamp(generation: &Uuid) -> Option<uuid::Timestamp> {
        let ts = decode_unix_timestamp(generation);
        let (seconds, nanos) = ts.to_unix();
        let (seconds, nanos) = (253370761200 - seconds, 999999999 - nanos);
        Some(uuid::Timestamp::from_unix(NoContext, seconds, nanos))
    }

    // Starts a new generation for this replicator instance
    pub fn new_generation(&mut self) {
        tracing::debug!("New generation started: {}", self.generation);
        self.set_generation(Self::generate_generation());
    }

    // Sets a generation for this replicator instance. This function
    // should be called if a generation number from S3-compatible storage
    // is reused in this session.
    pub fn set_generation(&mut self, generation: Uuid) {
        self.generation.swap(Arc::new(generation));
        self.commits_in_current_generation
            .store(0, Ordering::Release);
        self.reset_frames(0);
        tracing::debug!("Generation set to {}", self.generation);
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
    async fn read_change_counter(reader: &mut tokio::fs::File) -> Result<[u8; 4]> {
        let mut counter = [0u8; 4];
        reader.seek(std::io::SeekFrom::Start(24)).await?;
        reader.read_exact(&mut counter).await?;
        Ok(counter)
    }

    // Tries to read the local page size from the given database file
    async fn read_page_size(reader: &mut tokio::fs::File) -> Result<usize> {
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
    pub async fn compress_main_db_file(&self) -> Result<(&'static str, [u8; 4])> {
        let compressed_db = "db.gz";
        let mut reader = tokio::fs::File::open(&self.db_path).await?;
        let mut writer = async_compression::tokio::write::GzipEncoder::new(
            tokio::fs::File::create(compressed_db).await?,
        );
        let size = tokio::io::copy(&mut reader, &mut writer).await?;
        tracing::trace!(
            "Compressed database file ({} bytes) into {}",
            size,
            compressed_db
        );
        writer.shutdown().await?;
        let change_counter = Self::read_change_counter(&mut reader).await?;
        Ok((compressed_db, change_counter))
    }

    // Replicates local WAL pages to S3, if local WAL is present.
    // This function is called under the assumption that if local WAL
    // file is present, it was already detected to be newer than its
    // remote counterpart.
    pub async fn maybe_replicate_wal(&mut self) -> Result<()> {
        let wal_file = match WalFileReader::open(&format!("{}-wal", &self.db_path)).await {
            Ok(Some(file)) => file,
            _ => {
                tracing::info!("Local WAL not present - not replicating");
                return Ok(());
            }
        };

        self.store_metadata(wal_file.page_size(), wal_file.checksum())
            .await?;
        let frame_count = wal_file.frame_count().await;
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

    // Check if the local database file exists and contains data
    async fn main_db_exists_and_not_empty(&self) -> bool {
        let file = match tokio::fs::File::open(&self.db_path).await {
            Ok(file) => file,
            Err(_) => return false,
        };
        match file.metadata().await {
            Ok(metadata) => metadata.len() > 0,
            Err(_) => false,
        }
    }

    // Sends the main database file to S3 - if -wal file is present, it's replicated
    // too - it means that the local file was detected to be newer than its remote
    // counterpart.
    // returns whether the main db file was recovered.
    pub async fn snapshot_main_db_file(&mut self) -> Result<bool> {
        if !self.main_db_exists_and_not_empty().await {
            tracing::debug!("Not snapshotting, the main db file does not exist or is empty");
            return Ok(false);
        }
        tracing::debug!("Snapshotting {}", self.db_path);
        let start = Instant::now();
        let change_counter = match self.use_compression {
            CompressionKind::None => {
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(format!("{}-{}/db.db", self.db_name, self.generation))
                    .body(ByteStream::from_path(&self.db_path).await?)
                    .send()
                    .await?;
                let mut reader = tokio::fs::File::open(&self.db_path).await?;
                Self::read_change_counter(&mut reader).await?
            }
            CompressionKind::Gzip => {
                // TODO: find a way to compress ByteStream on the fly instead of creating
                // an intermediary file.
                let (compressed_db_path, change_counter) = self.compress_main_db_file().await?;
                let key = format!("{}-{}/db.gz", self.db_name, self.generation);
                self.client
                    .put_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .body(ByteStream::from_path(compressed_db_path).await?)
                    .send()
                    .await?;
                let _ = tokio::fs::remove_file(compressed_db_path).await;
                change_counter
            }
        };

        /* FIXME: we can't rely on the change counter in WAL mode:
         ** "In WAL mode, changes to the database are detected using the wal-index and
         ** so the change counter is not needed. Hence, the change counter might not be
         ** incremented on each transaction in WAL mode."
         ** Instead, we need to consult WAL checksums.
         */
        let change_counter_key = format!("{}-{}/.changecounter", self.db_name, self.generation);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(change_counter_key)
            .body(ByteStream::from(Bytes::copy_from_slice(&change_counter)))
            .send()
            .await?;
        tracing::debug!("Main db snapshot complete in {:?}", Instant::now() - start);
        Ok(true)
    }

    // Returns newest replicated generation, or None, if one is not found.
    // FIXME: assumes that this bucket stores *only* generations for databases,
    // it should be more robust and continue looking if the first item does not
    // match the <db-name>-<generation-uuid>/ pattern.
    pub async fn latest_generation_before(
        &self,
        timestamp: Option<&DateTime<Utc>>,
    ) -> Option<Uuid> {
        let mut next_marker: Option<String> = None;
        let prefix = format!("{}-", self.db_name);
        let threshold = timestamp.map(|ts| ts.timestamp() as u64);
        loop {
            let mut request = self.list_objects().prefix(prefix.clone());
            if threshold.is_none() {
                request = request.max_keys(1);
            }
            if let Some(marker) = next_marker.take() {
                request = request.marker(marker);
            }
            let response = request.send().await.ok()?;
            let objs = response.contents()?;
            if objs.is_empty() {
                break;
            }
            let mut last_key = None;
            let mut last_gen = None;
            for obj in objs {
                let key = obj.key();
                last_key = key;
                if let Some(key) = last_key {
                    let key = match key.find('/') {
                        Some(index) => &key[self.db_name.len() + 1..index],
                        None => key,
                    };
                    if Some(key) != last_gen {
                        last_gen = Some(key);
                        if let Ok(generation) = Uuid::parse_str(key) {
                            match threshold.as_ref() {
                                None => return Some(generation),
                                Some(threshold) => match Self::generation_to_timestamp(&generation)
                                {
                                    None => {
                                        tracing::warn!(
                                            "Generation {} is not valid UUID v7",
                                            generation
                                        );
                                    }
                                    Some(ts) => {
                                        let (unix_seconds, _) = ts.to_unix();
                                        if tracing::enabled!(tracing::Level::DEBUG) {
                                            let ts = Utc
                                                .timestamp_millis_opt((unix_seconds * 1000) as i64)
                                                .unwrap()
                                                .to_rfc3339();
                                            tracing::debug!(
                                                "Generation candidate: {} - timestamp: {}",
                                                generation,
                                                ts
                                            );
                                        }
                                        if &unix_seconds <= threshold {
                                            return Some(generation);
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
            next_marker = last_key.map(String::from);
        }
        None
    }

    // Tries to fetch the remote database change counter from given generation
    pub async fn get_remote_change_counter(&self, generation: &Uuid) -> Result<[u8; 4]> {
        let mut remote_change_counter = [0u8; 4];
        if let Ok(response) = self
            .get_object(format!("{}-{}/.changecounter", self.db_name, generation))
            .send()
            .await
        {
            response
                .body
                .collect()
                .await?
                .copy_to_slice(&mut remote_change_counter)
        }
        Ok(remote_change_counter)
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

    // Parses the frame and page number from given key.
    // Format: <db-name>-<generation>/<first-frame-no>-<last-frame-no>-<timestamp>.<compression-kind>
    fn parse_frame_range(key: &str) -> Option<(u32, u32, u64, CompressionKind)> {
        let frame_delim = key.rfind('/')?;
        let frame_suffix = &key[(frame_delim + 1)..];
        let timestamp_delim = frame_suffix.rfind('-')?;
        let last_frame_delim = frame_suffix[..timestamp_delim].rfind('-')?;
        let compression_delim = frame_suffix.rfind('.')?;
        let first_frame_no = frame_suffix[0..last_frame_delim].parse::<u32>().ok()?;
        let last_frame_no = frame_suffix[(last_frame_delim + 1)..timestamp_delim]
            .parse::<u32>()
            .ok()?;
        let timestamp = frame_suffix[(timestamp_delim + 1)..compression_delim]
            .parse::<u64>()
            .ok()?;
        let compression_kind =
            CompressionKind::parse(&frame_suffix[(compression_delim + 1)..]).ok()?;
        Some((first_frame_no, last_frame_no, timestamp, compression_kind))
    }

    // Restores the database state from given remote generation
    pub async fn restore_from(
        &mut self,
        generation: Uuid,
        utc_time: Option<DateTime<Utc>>,
    ) -> Result<RestoreAction> {
        if let Some(tombstone) = self.get_tombstone().await? {
            if let Some(timestamp) = Self::generation_to_timestamp(&generation) {
                if tombstone.timestamp() as u64 >= timestamp.to_unix().0 {
                    tracing::error!(
                        "Restoration failed. Database '{}' has been tombstoned at {}.",
                        self.db_name,
                        tombstone
                    );
                    return Ok(RestoreAction::None);
                }
            }
        }

        // first check if there are any remaining files that we didn't manage to upload
        // on time in the last run
        self.upload_remaining_files(&generation).await?;

        // Check if the database needs to be restored by inspecting the database
        // change counter and the WAL size.
        let local_counter = match tokio::fs::File::open(&self.db_path).await {
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

        let remote_counter = self.get_remote_change_counter(&generation).await?;
        tracing::debug!("Counters: l={:?}, r={:?}", local_counter, remote_counter);

        let last_consistent_frame = self.get_last_consistent_frame(&generation).await?;
        tracing::debug!(
            "Last consistent remote frame in generation {}: {}.",
            generation,
            last_consistent_frame
        );

        let wal_pages = self.get_local_wal_page_count().await;
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
                        return Ok(RestoreAction::ReuseGeneration(generation));
                    }
                    std::cmp::Ordering::Greater => {
                        tracing::info!("Local change counter matches the remote one, but local WAL contains newer data, which needs to be replicated");
                        return Ok(RestoreAction::SnapshotMainDbFile);
                    }
                    std::cmp::Ordering::Less => (),
                }
            }
            std::cmp::Ordering::Greater => {
                tracing::info!("Local change counter is larger than its remote counterpart - a new snapshot needs to be replicated");
                return Ok(RestoreAction::SnapshotMainDbFile);
            }
            std::cmp::Ordering::Less => (),
        }

        tokio::fs::rename(&self.db_path, format!("{}.bottomless.backup", self.db_path))
            .await
            .ok(); // Best effort
        let mut main_db_writer = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.db_path)
            .await?;
        // If the db file is not present, the database could have been empty

        let main_db_path = match self.use_compression {
            CompressionKind::None => format!("{}-{}/db.db", self.db_name, generation),
            CompressionKind::Gzip => format!("{}-{}/db.gz", self.db_name, generation),
        };

        if let Ok(db_file) = self.get_object(main_db_path).send().await {
            let mut body_reader = db_file.body.into_async_read();
            let db_size = match self.use_compression {
                CompressionKind::None => {
                    tokio::io::copy(&mut body_reader, &mut main_db_writer).await?
                }
                CompressionKind::Gzip => {
                    let mut decompress_reader = async_compression::tokio::bufread::GzipDecoder::new(
                        tokio::io::BufReader::new(body_reader),
                    );
                    tokio::io::copy(&mut decompress_reader, &mut main_db_writer).await?
                }
            };
            main_db_writer.flush().await?;

            let page_size = Self::read_page_size(&mut main_db_writer).await?;
            self.set_page_size(page_size)?;
            tracing::info!("Restored the main database file ({} bytes)", db_size);
        }

        let mut next_marker = None;
        let prefix = format!("{}-{}/", self.db_name, generation);
        tracing::debug!("Overwriting any existing WAL file: {}-wal", &self.db_path);
        tokio::fs::remove_file(&format!("{}-wal", &self.db_path))
            .await
            .ok();
        tokio::fs::remove_file(&format!("{}-shm", &self.db_path))
            .await
            .ok();

        let mut applied_wal_frame = false;
        if let Some((page_size, mut checksum)) = self.get_metadata(&generation).await? {
            self.set_page_size(page_size as usize)?;
            let mut page_buf = {
                let mut v = Vec::with_capacity(page_size as usize);
                v.spare_capacity_mut();
                unsafe { v.set_len(page_size as usize) };
                v
            };
            'restore_wal: loop {
                let mut list_request = self.list_objects().prefix(&prefix);
                if let Some(marker) = next_marker {
                    list_request = list_request.marker(marker);
                }
                let response = list_request.send().await?;
                let objs = match response.contents() {
                    Some(objs) => objs,
                    None => {
                        tracing::debug!("No objects found in generation {}", generation);
                        break;
                    }
                };
                let mut pending_pages = TransactionPageCache::new(
                    self.restore_transaction_page_swap_after,
                    page_size,
                    self.restore_transaction_cache_fpath.clone(),
                );
                let mut last_received_frame_no = 0;
                for obj in objs {
                    let key = obj
                        .key()
                        .ok_or_else(|| anyhow::anyhow!("Failed to get key for an object"))?;
                    tracing::debug!("Loading {}", key);

                    let (first_frame_no, last_frame_no, timestamp, compression_kind) =
                        match Self::parse_frame_range(key) {
                            Some(result) => result,
                            None => {
                                if !key.ends_with(".gz")
                                    && !key.ends_with(".db")
                                    && !key.ends_with(".meta")
                                    && !key.ends_with(".changecounter")
                                {
                                    tracing::warn!("Failed to parse frame/page from key {}", key);
                                }
                                continue;
                            }
                        };
                    if first_frame_no != last_received_frame_no + 1 {
                        tracing::warn!("Missing series of consecutive frames. Last applied frame: {}, next found: {}. Stopping the restoration process", 
                            last_received_frame_no, first_frame_no);
                        break;
                    }
                    if last_frame_no > last_consistent_frame {
                        tracing::warn!("Remote log contains frame {} larger than last consistent frame ({}), stopping the restoration process",
                                last_frame_no, last_consistent_frame);
                        break;
                    }
                    if let Some(threshold) = utc_time.as_ref() {
                        match Utc.timestamp_millis_opt((timestamp * 1000) as i64) {
                            LocalResult::Single(timestamp) => {
                                if &timestamp > threshold {
                                    tracing::info!("Frame batch {} has timestamp more recent than expected {}. Stopping recovery.", key, timestamp.to_rfc3339());
                                    break 'restore_wal; // reached end of restoration timestamp
                                }
                            }
                            _ => {
                                tracing::trace!("Couldn't parse requested frame batch {} timestamp. Stopping recovery.", key);
                                break 'restore_wal;
                            }
                        }
                    }
                    let frame = self.get_object(key.into()).send().await?;
                    let mut frameno = first_frame_no;
                    let mut reader =
                        BatchReader::new(frameno, frame.body, self.page_size, compression_kind);
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
                            pending_pages.flush(&mut main_db_writer).await?;
                            applied_wal_frame = true;
                        }
                        frameno += 1;
                        last_received_frame_no += 1;
                    }
                    main_db_writer.flush().await?;
                }
                next_marker = response
                    .is_truncated()
                    .then(|| objs.last().map(|elem| elem.key().unwrap().to_string()))
                    .flatten();
                if next_marker.is_none() {
                    tracing::trace!("Restored DB from S3 backup using generation {}", generation);
                    break;
                }
            }
        } else {
            tracing::info!(".meta object not found, skipping WAL restore.");
        }

        main_db_writer.shutdown().await?;
        tracing::info!("Finished database restoration");

        if applied_wal_frame {
            Ok::<_, anyhow::Error>(RestoreAction::SnapshotMainDbFile)
        } else {
            // since WAL was not applied, we can reuse the latest generation
            Ok::<_, anyhow::Error>(RestoreAction::ReuseGeneration(generation))
        }
    }

    // Restores the database state from newest remote generation
    pub async fn restore(
        &mut self,
        generation: Option<Uuid>,
        timestamp: Option<DateTime<Utc>>,
    ) -> Result<RestoreAction> {
        let generation = match generation {
            Some(gen) => gen,
            None => match self.latest_generation_before(timestamp.as_ref()).await {
                Some(gen) => gen,
                None => {
                    tracing::debug!("No generation found, nothing to restore");
                    return Ok(RestoreAction::SnapshotMainDbFile);
                }
            },
        };

        tracing::info!("Restoring from generation {}", generation);
        self.restore_from(generation, timestamp).await
    }

    pub async fn get_last_consistent_frame(&self, generation: &Uuid) -> Result<u32> {
        let prefix = format!("{}-{}/", self.db_name, generation);
        let mut marker: Option<String> = None;
        let mut last_frame = 0;
        while {
            let mut list_objects = self.list_objects().prefix(&prefix);
            if let Some(marker) = marker.take() {
                list_objects = list_objects.marker(marker);
            }
            let response = list_objects.send().await?;
            marker = Self::try_get_last_frame_no(response, &mut last_frame);
            marker.is_some()
        } {}
        Ok(last_frame)
    }

    fn try_get_last_frame_no(response: ListObjectsOutput, frame_no: &mut u32) -> Option<String> {
        let objs = response.contents()?;
        let mut last_key = None;
        for obj in objs.iter() {
            last_key = Some(obj.key()?);
            if let Some(key) = last_key {
                if let Some((_, last_frame_no, _, _)) = Self::parse_frame_range(key) {
                    *frame_no = last_frame_no;
                }
            }
        }
        last_key.map(String::from)
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
                    let bucket = self.bucket.clone();
                    let key = key.to_string();
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        let body = ByteStream::from_path(&fpath).await.unwrap();
                        if let Err(e) = client
                            .put_object()
                            .bucket(bucket)
                            .key(key.clone())
                            .body(body)
                            .send()
                            .await
                        {
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
        if str.ends_with(".gz") | str.ends_with(".raw") | str.ends_with(".meta") {
            let idx = str.rfind(dir)?;
            return Some(&str[idx..]);
        }
        None
    }

    pub async fn store_metadata(&self, page_size: u32, crc: u64) -> Result<()> {
        let key = format!("{}-{}/.meta", self.db_name, self.generation.load());
        put_metadata_obj(&self.client, &self.bucket, key, page_size, crc).await
    }

    pub async fn get_metadata(&self, generation: &Uuid) -> Result<Option<(u32, u64)>> {
        let key = format!("{}-{}/.meta", self.db_name, generation);
        if let Ok(obj) = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            let mut data = obj.body.collect().await?;
            let page_size = data.get_u32();
            let crc = data.get_u64();
            Ok(Some((page_size, crc)))
        } else {
            Ok(None)
        }
    }

    /// Marks current replicator database as deleted, invalidating all generations.
    pub async fn delete_all(&self, older_than: Option<NaiveDateTime>) -> Result<DeleteAll> {
        tracing::info!(
            "Called for tombstoning of all contents of the '{}' database",
            self.db_name
        );
        let key = format!("{}.tombstone", self.db_name);
        let threshold = older_than.unwrap_or(NaiveDateTime::MAX);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(
                threshold.timestamp().to_be_bytes().to_vec(),
            ))
            .send()
            .await?;
        let delete_task = DeleteAll::new(
            self.client.clone(),
            self.bucket.clone(),
            self.db_name.clone(),
            threshold,
        );
        Ok(delete_task)
    }

    /// Checks if current replicator database has been marked as deleted.
    pub async fn get_tombstone(&self) -> Result<Option<NaiveDateTime>> {
        let key = format!("{}.tombstone", self.db_name);
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await;
        match resp {
            Ok(out) => {
                let mut buf = [0u8; 8];
                out.body.collect().await?.copy_to_slice(&mut buf);
                let timestamp = i64::from_be_bytes(buf);
                let tombstone = NaiveDateTime::from_timestamp_opt(timestamp, 0);
                Ok(tombstone)
            }
            Err(SdkError::ServiceError(se)) => match se.into_err() {
                GetObjectError::NoSuchKey(_) => Ok(None),
                e => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        }
    }
}

/// This structure is returned by [Replicator::delete_all] after tombstoning (soft deletion) has
/// been confirmed. It may be called using [DeleteAll::commit] to trigger a follow up procedure that
/// performs hard deletion of corresponding S3 objects.
#[derive(Debug)]
pub struct DeleteAll {
    client: Client,
    bucket: String,
    db_name: String,
    threshold: NaiveDateTime,
}

impl DeleteAll {
    fn new(client: Client, bucket: String, db_name: String, threshold: NaiveDateTime) -> Self {
        DeleteAll {
            client,
            bucket,
            db_name,
            threshold,
        }
    }

    pub fn threshold(&self) -> &NaiveDateTime {
        &self.threshold
    }

    /// Performs hard deletion of all bottomless generations older than timestamp provided in
    /// current request.
    pub async fn commit(self) -> Result<u32> {
        let mut next_marker = None;
        let mut removed_count = 0;
        loop {
            let mut list_request = self
                .client
                .list_objects()
                .bucket(&self.bucket)
                .set_delimiter(Some("/".to_string()))
                .prefix(&self.db_name);

            if let Some(marker) = next_marker {
                list_request = list_request.marker(marker)
            }

            let response = list_request.send().await?;
            let prefixes = match response.common_prefixes() {
                Some(prefixes) => prefixes,
                None => {
                    tracing::debug!("no generations found to delete");
                    return Ok(0);
                }
            };

            for prefix in prefixes {
                if let Some(prefix) = &prefix.prefix {
                    let prefix = &prefix[self.db_name.len() + 1..prefix.len() - 1];
                    let uuid = Uuid::try_parse(prefix)?;
                    if let Some(datetime) = Replicator::generation_to_timestamp(&uuid) {
                        if datetime.to_unix().0 >= self.threshold.timestamp() as u64 {
                            continue;
                        }
                        tracing::debug!("Removing generation {}", uuid);
                        self.remove(uuid).await?;
                        removed_count += 1;
                    }
                }
            }

            next_marker = response.next_marker().map(|s| s.to_owned());
            if next_marker.is_none() {
                break;
            }
        }
        tracing::debug!("Removed {} generations", removed_count);
        self.remove_tombstone().await?;
        Ok(removed_count)
    }

    pub async fn remove_tombstone(&self) -> Result<()> {
        let key = format!("{}.tombstone", self.db_name);
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        Ok(())
    }

    async fn remove(&self, generation: Uuid) -> Result<()> {
        let mut removed = 0;
        let mut next_marker = None;
        loop {
            let mut list_request = self
                .client
                .list_objects()
                .bucket(&self.bucket)
                .prefix(format!("{}-{}/", &self.db_name, generation));

            if let Some(marker) = next_marker {
                list_request = list_request.marker(marker)
            }

            let response = list_request.send().await?;
            let objs = match response.contents() {
                Some(prefixes) => prefixes,
                None => {
                    return Ok(());
                }
            };

            for obj in objs {
                if let Some(key) = obj.key() {
                    tracing::trace!("Removing {}", key);
                    self.client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .send()
                        .await?;
                    removed += 1;
                }
            }

            next_marker = response.next_marker().map(|s| s.to_owned());
            if next_marker.is_none() {
                tracing::trace!("Removed {} snapshot generations", removed);
                return Ok(());
            }
        }
    }
}

async fn put_metadata_obj(
    client: &Client,
    bucket: &str,
    key: String,
    page_size: u32,
    crc: u64,
) -> Result<()> {
    tracing::debug!(
        "Storing metadata at '{}': page size - {}, crc - {}",
        key,
        page_size,
        crc
    );
    let mut body = BytesMut::with_capacity(12);
    body.extend_from_slice(page_size.to_be_bytes().as_slice());
    body.extend_from_slice(crc.to_be_bytes().as_slice());
    let _ = client
        .put_object()
        .bucket(bucket)
        .key(key)
        .body(ByteStream::from(body.freeze()))
        .send()
        .await?;
    Ok(())
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
