use std::{
    io::{self, Cursor, Read, SeekFrom},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::{
    content::{
        manager::DownloaderError,
        zip::{CompressionType, ZipFile, ZipFileEntry},
    },
    util::native::{SafeParent, maxima_dir},
};
use async_trait::async_trait;
use derive_getters::Getters;
use flate2::{Decompress, bufread::DeflateDecoder as BufreadDeflateDecoder};
use futures::{Stream, StreamExt, TryStreamExt};
use log::{debug, error, warn};
use reqwest::Client;
use thiserror::Error;
use tokio::{
    fs::{File, OpenOptions, create_dir, create_dir_all},
    io::{AsyncSeekExt, AsyncWrite, BufReader, BufWriter},
    sync::Mutex as AsyncMutex,
};
use tokio_util::compat::FuturesAsyncReadCompatExt;

async fn zstate_path(id: &str, path: &str) -> Result<PathBuf, DownloaderError> {
    let mut path = maxima_dir()?.join("temp/downloader").join(id).join(path);
    path.set_extension("eazstate");
    tokio::fs::create_dir_all(path.safe_parent()?).await?;
    Ok(path)
}

trait DownloadDecoder: Send {
    fn save_state(&mut self, buf: &mut BytesMut);
    fn restore_state(&mut self, buf: &mut Bytes);
    fn seek(&mut self, pos: SeekFrom) -> Result<(), DownloaderError>;
    fn write_in_pos(&self) -> u64;
    fn write_out_pos(&self) -> u64;
    fn get_mut(&mut self) -> Arc<AsyncMutex<BufWriter<File>>>;
    fn supports_resume(&self) -> bool {
        true
    }
}

struct ZLibDeflateDecoder {
    decompress: flate2::Decompress,
    writer: Arc<AsyncMutex<BufWriter<File>>>,
}

impl ZLibDeflateDecoder {
    fn new(writer: BufWriter<File>) -> Self {
        Self {
            decompress: Decompress::new(true),
            writer: Arc::new(AsyncMutex::new(writer)),
        }
    }
}

impl DownloadDecoder for ZLibDeflateDecoder {
    fn save_state(&mut self, buf: &mut BytesMut) {
        // State serialization disabled — flate2 doesn't expose raw zlib state
        let _ = buf;
    }

    fn restore_state(&mut self, buf: &mut Bytes) {
        // State deserialization disabled — flate2 doesn't expose raw zlib state
        self.decompress.reset(false);
        let _ = buf;
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<(), DownloaderError> {
        let writer = self.writer.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(async move { writer.lock().await.seek(pos).await })
        })?;
        Ok(())
    }

    fn write_in_pos(&self) -> u64 {
        self.decompress.total_in()
    }

    fn write_out_pos(&self) -> u64 {
        self.decompress.total_out()
    }

    fn get_mut(&mut self) -> Arc<AsyncMutex<BufWriter<File>>> {
        self.writer.clone()
    }

    fn supports_resume(&self) -> bool {
        false
    }
}

struct NoopDecoder {
    writer: Arc<AsyncMutex<BufWriter<File>>>,
    pos: u64,
}

impl NoopDecoder {
    pub fn new(writer: BufWriter<File>) -> Self {
        Self {
            writer: Arc::new(AsyncMutex::new(writer)),
            pos: 0,
        }
    }
}

impl DownloadDecoder for NoopDecoder {
    fn save_state(&mut self, buf: &mut BytesMut) {
        buf.put_u64(self.pos);
    }

    fn restore_state(&mut self, buf: &mut Bytes) {
        self.pos = buf.get_u64();
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<(), DownloaderError> {
        let writer = self.writer.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                let mut file = writer.lock().await;
                file.seek(pos).await
            })
        })?;
        Ok(())
    }

    fn write_in_pos(&self) -> u64 {
        self.pos
    }

    fn write_out_pos(&self) -> u64 {
        self.pos
    }

    fn get_mut(&mut self) -> Arc<AsyncMutex<BufWriter<File>>> {
        self.writer.clone()
    }
}

struct AsyncWriterWrapper<'a> {
    decoder: &'a mut Box<dyn DownloadDecoder>,
    inner: Arc<AsyncMutex<BufWriter<File>>>,
}

impl<'a> AsyncWriterWrapper<'a> {
    async fn new(decoder: &'a mut Box<dyn DownloadDecoder>) -> Self {
        let inner = decoder.get_mut();
        AsyncWriterWrapper { decoder, inner }
    }
}

impl<'a> AsyncWrite for AsyncWriterWrapper<'a> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> task::Poll<Result<usize, io::Error>> {
        match self.inner.try_lock() {
            Ok(mut guard) => Pin::new(&mut *guard).poll_write(cx, buf),
            Err(_) => {
                cx.waker().wake_by_ref();
                task::Poll::Pending
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), io::Error>> {
        match self.inner.try_lock() {
            Ok(mut guard) => Pin::new(&mut *guard).poll_flush(cx),
            Err(_) => {
                cx.waker().wake_by_ref();
                task::Poll::Pending
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Result<(), io::Error>> {
        match self.inner.try_lock() {
            Ok(mut guard) => Pin::new(&mut *guard).poll_shutdown(cx),
            Err(_) => {
                cx.waker().wake_by_ref();
                task::Poll::Pending
            }
        }
    }
}

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("download failed after {0} bytes")]
    DownloadFailed(usize),
    #[error("failed to download chunk `{entry}`: {error}")]
    ChunkDownload {
        entry: String,
        error: reqwest::Error,
    },
    #[error("failed to copy chunk `{entry}`: {error}")]
    ChunkCopy {
        entry: String,
        error: std::io::Error,
    },
}

#[derive(PartialEq, Debug)]
enum EntryDownloadState {
    Fresh,
    Resumable,
    Complete,
    Borked,
}

struct DownloadContext {
    id: String,
    path: PathBuf,
}

type BytesDownloadedCallback = Box<dyn Fn(usize) + Send + Sync>;

struct EntryDownloadRequest<'a> {
    context: &'a DownloadContext,
    url: &'a str,
    entry: &'a ZipFileEntry,
    client: Client,
    decoder: Box<dyn DownloadDecoder>,
    callback: Option<BytesDownloadedCallback>,
}

impl<'a> EntryDownloadRequest<'a> {
    pub fn new(
        context: &'a DownloadContext,
        url: &'a str,
        entry: &'a ZipFileEntry,
        client: Client,
        decoder: Box<dyn DownloadDecoder>,
        callback: Option<BytesDownloadedCallback>,
    ) -> Self {
        Self {
            context,
            url,
            entry,
            client,
            decoder,
            callback,
        }
    }

    async fn state(
        context: &DownloadContext,
        entry: &ZipFileEntry,
    ) -> Result<EntryDownloadState, DownloaderError> {
        let path = context.path.join(entry.name());

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EntryDownloadState::Fresh);
            }
            Err(e) => return Err(e.into()),
        };

        let file_size = metadata.len() as i64;
        if file_size == 0 {
            return Ok(EntryDownloadState::Fresh);
        }

        let entry_size = *entry.uncompressed_size();
        let size_match = entry_size == file_size;

        if !size_match {
            warn!(
                "Size mismatch for {}: expected={} actual={}",
                entry.name(),
                entry_size,
                file_size
            );
            if file_size > entry_size {
                return Ok(EntryDownloadState::Borked);
            }
            return Ok(EntryDownloadState::Resumable);
        }

        Ok(EntryDownloadState::Complete)
    }

    async fn download(&mut self) -> Result<(), DownloadError> {
        let mut last_err = None;

        for attempt in 1..=5 {
            let start = self.decoder.write_in_pos() as i64;
            let end = *self.entry.compressed_size();

            debug!(
                "Downloading {} bytes={}-{} (uncompressed: {}), attempt {}/5",
                self.entry.name(),
                start,
                end,
                self.entry.uncompressed_size(),
                attempt,
            );

            match self.download_range(start, end).await {
                Ok(()) => return Ok(()),
                Err(DownloaderError::Download(err)) => {
                    warn!(
                        "Download attempt {}/5 failed for {}: {}",
                        attempt,
                        self.entry.name(),
                        err
                    );
                    last_err = Some(err);
                }
                Err(other) => {
                    warn!(
                        "Download attempt {}/5 failed for {}: {}",
                        attempt,
                        self.entry.name(),
                        other
                    );
                    last_err = Some(DownloadError::DownloadFailed(
                        self.decoder.write_in_pos() as usize
                    ));
                }
            }
        }

        Err(last_err.unwrap_or(DownloadError::DownloadFailed(
            self.decoder.write_in_pos() as usize
        )))
    }

    /// End is not inclusive
    pub async fn download_range(&mut self, start: i64, end: i64) -> Result<(), DownloaderError> {
        let offset = self.entry.data_offset();
        let range = format!("bytes={}-{}", offset + start, offset + end - 1);

        let response = match self
            .client
            .get(self.url)
            .header("range", range)
            .send()
            .await
        {
            Ok(res) => res,
            Err(err) => {
                return Err(DownloaderError::Download(DownloadError::ChunkDownload {
                    entry: self.entry.name().clone(),
                    error: err,
                }));
            }
        };

        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            && response.status() != reqwest::StatusCode::OK
        {
            return Err(DownloaderError::Http(response.status()));
        }

        let stream = response.bytes_stream();
        let counting_stream = ByteCountingStream::new(stream, self.callback.as_ref());

        let writer_arc = self.decoder.get_mut();
        let mut writer = writer_arc.lock().await;

        match self.entry.compression_type() {
            CompressionType::None => {
                // No decompression — stream straight to file
                let async_read = counting_stream.into_async_read();
                let mut reader = BufReader::new(async_read.compat());
                if let Err(err) = tokio::io::copy(&mut reader, &mut *writer).await {
                    return Err(DownloaderError::Download(DownloadError::ChunkCopy {
                        entry: self.entry.name().clone(),
                        error: err,
                    }));
                }
            }
            CompressionType::Deflate => {
                // Collect compressed bytes then decompress — avoids async/sync boundary issues with flate2
                let compressed: Vec<u8> = counting_stream
                    .try_fold(Vec::new(), |mut acc, chunk| async move {
                        acc.extend_from_slice(&chunk);
                        Ok(acc)
                    })
                    .await
                    .map_err(|e| DownloaderError::Io(e))?;

                let mut decoder = BufreadDeflateDecoder::new(Cursor::new(&compressed));
                let mut decompressed = Vec::with_capacity(*self.entry.uncompressed_size() as usize);
                decoder.read_to_end(&mut decompressed)?;

                let mut cursor = Cursor::new(decompressed);
                if let Err(err) = tokio::io::copy(&mut cursor, &mut *writer).await {
                    return Err(DownloaderError::Download(DownloadError::ChunkCopy {
                        entry: self.entry.name().clone(),
                        error: err,
                    }));
                }
            }
        }

        Ok(())
    }
}

#[derive(Getters)]
pub struct ZipDownloader {
    id: String,
    url: String,
    path: PathBuf,
    client: Client,
    manifest: ZipFile,
}

impl ZipDownloader {
    pub async fn new<P: AsRef<Path>>(
        id: &str,
        zip_url: &str,
        path: P,
    ) -> Result<Self, DownloaderError>
    where
        PathBuf: From<P>,
    {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(DownloaderError::PathNotAbsolute(path));
        }

        let manifest = ZipFile::fetch(zip_url).await?;

        Ok(Self {
            id: id.to_owned(),
            url: zip_url.to_owned(),
            path,
            client: Client::builder().build()?,
            manifest,
        })
    }

    pub async fn read_zip_entry_bytes(
        &self,
        entry: &ZipFileEntry,
        length: u64,
    ) -> Result<Bytes, DownloaderError> {
        let offset = entry.data_offset();
        let compressed_size = *entry.compressed_size();

        let range_header = format!("bytes={}-{}", offset, offset + compressed_size - 1);

        let response = self
            .client
            .get(&self.url)
            .header("Range", range_header)
            .send()
            .await?;

        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT
            && response.status() != reqwest::StatusCode::OK
        {
            return Err(DownloaderError::Http(response.status()));
        }

        let compressed_data = response.bytes().await?;
        let decompressed_data = match entry.compression_type() {
            CompressionType::None => {
                let entry_size = *entry.uncompressed_size() as u64;
                let available_length = std::cmp::min(length, entry_size);

                if available_length > compressed_data.len() as u64 {
                    return Err(DownloaderError::EntrySize {
                        requested: available_length,
                        entry: compressed_data.len(),
                    });
                }

                Bytes::copy_from_slice(&compressed_data[..available_length as usize])
            }
            CompressionType::Deflate => {
                let decoder = BufreadDeflateDecoder::new(Cursor::new(&compressed_data));
                let mut limited_reader = decoder.take(length);
                let mut decompressed_data = Vec::with_capacity(length as usize);
                limited_reader.read_to_end(&mut decompressed_data)?;
                Bytes::from(decompressed_data)
            }
        };

        Ok(decompressed_data)
    }

    pub async fn download_single_file(
        &self,
        entry: &ZipFileEntry,
        callback: Option<BytesDownloadedCallback>,
    ) -> Result<usize, DownloaderError> {
        let file_path = self.path.join(entry.name());

        create_dir_all(file_path.safe_parent()?).await?;

        if entry.name().ends_with('/') {
            debug!("{} is a directory", entry.name());
            match create_dir(&file_path).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(0);
        }

        if *entry.uncompressed_size() == 0 {
            debug!("{} is empty", entry.name());
            return Ok(0);
        }

        let offset = entry.data_offset();
        debug!("Type: {:?}", entry.compression_type());
        debug!("Compressed Size: {}", entry.compressed_size());
        debug!("Offset: {}", offset);

        let context = DownloadContext {
            id: self.id.to_owned(),
            path: self.path.clone(),
        };

        let state = EntryDownloadRequest::state(&context, entry).await?;
        if state == EntryDownloadState::Complete {
            if let Some(callback) = callback {
                callback(*entry.compressed_size() as usize);
            }
            return Ok(0);
        }

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&file_path)
            .await?;

        if state == EntryDownloadState::Borked {
            warn!("Found borked file {}", entry.name());
            file.set_len(*entry.uncompressed_size() as u64).await?;
        }

        let writer = tokio::io::BufWriter::new(file);

        let mut decoder: Box<dyn DownloadDecoder> = match entry.compression_type() {
            CompressionType::None => Box::new(NoopDecoder::new(writer)),
            CompressionType::Deflate => Box::new(ZLibDeflateDecoder::new(writer)),
        };

        if state == EntryDownloadState::Resumable {
            if decoder.supports_resume() {
                let state_file = zstate_path(&self.id, entry.name()).await?;
                if state_file.exists() {
                    let mut buf = Bytes::from(tokio::fs::read(state_file).await?);
                    decoder.restore_state(&mut buf);
                } else {
                    tokio::fs::create_dir_all(state_file.safe_parent()?).await?;
                }
            } else {
                warn!(
                    "Decoder for {} does not support resume, restarting from scratch",
                    entry.name()
                );
                decoder.get_mut().lock().await.get_mut().set_len(0).await?;
                decoder.seek(SeekFrom::Start(0))?;
            }
        }

        let mut request = EntryDownloadRequest::new(
            &context,
            &self.url,
            entry,
            self.client.clone(),
            decoder,
            callback,
        );

        request.download().await?;
        Ok(0)
    }
}

struct ByteCountingStream<'a, S> {
    inner: S,
    byte_count: usize,
    callback: Option<&'a BytesDownloadedCallback>,
}

impl<'a, S> ByteCountingStream<'a, S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>>,
{
    fn new(inner: S, callback: Option<&'a BytesDownloadedCallback>) -> Self {
        ByteCountingStream {
            inner,
            byte_count: 0,
            callback,
        }
    }
}

impl<'a, S> Stream for ByteCountingStream<'a, S>
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    type Item = Result<bytes::Bytes, tokio::io::Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                self.byte_count += chunk.len();
                if let Some(callback) = &self.callback {
                    callback(chunk.len());
                }
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            std::task::Poll::Ready(Some(Err(err))) => {
                error!("Downloader error: {:?}", err);
                std::task::Poll::Ready(Some(Err(futures::io::Error::other(
                    DownloadError::DownloadFailed(self.byte_count),
                ))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}
