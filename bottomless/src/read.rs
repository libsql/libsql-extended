use crate::replicator::CompressionKind;
use crate::wal::WalFrameHeader;
use anyhow::Result;
use async_compression::tokio::bufread::{GzipDecoder, XzEncoder};
use aws_sdk_s3::primitives::ByteStream;
use std::io::ErrorKind;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio_util::io::StreamReader;

type AsyncByteReader = dyn AsyncRead + Send + Sync;

pub(crate) struct BatchReader {
    reader: Pin<Box<AsyncByteReader>>,
    next_frame_no: u32,
}

impl BatchReader {
    pub fn new(
        init_frame_no: u32,
        content: ByteStream,
        page_size: usize,
        use_compression: CompressionKind,
    ) -> Self {
        let reader =
            BufReader::with_capacity(page_size + WalFrameHeader::SIZE, StreamReader::new(content));
        BatchReader {
            next_frame_no: init_frame_no,
            reader: match use_compression {
                CompressionKind::None => Box::pin(reader),
                CompressionKind::Gzip => {
                    let gzip = GzipDecoder::new(reader);
                    Box::pin(gzip)
                }
                CompressionKind::Xz => {
                    let xz = XzEncoder::new(reader);
                    Box::pin(xz)
                }
            },
        }
    }

    /// Reads next frame header without frame body (WAL page).
    pub(crate) async fn next_frame_header(&mut self) -> Result<Option<WalFrameHeader>> {
        let mut buf = [0u8; WalFrameHeader::SIZE];
        let res = self.reader.read_exact(&mut buf).await;
        match res {
            Ok(_) => Ok(Some(WalFrameHeader::from(buf))),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Reads the next frame stored in a current batch.
    /// Returns a frame number or `None` if no frame was remaining in the buffer.
    pub(crate) async fn next_page(&mut self, page_buf: &mut [u8]) -> Result<()> {
        self.reader.read_exact(page_buf).await?;
        self.next_frame_no += 1;
        Ok(())
    }
}
