use std::{
    fs::{self, File, OpenOptions}, io::{self, BufReader, BufWriter, Read, Write}, path::{Path, PathBuf}, sync::Arc, time::Duration,
};

use miniz_oxide::{
    DataFormat, MZError, MZFlush, MZStatus,
    inflate::stream::{InflateState, inflate},
};
use reqwest::{
    StatusCode,
    blocking::{Client, Response},
    header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE, ETAG, IF_RANGE, RANGE},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;
use crate::content::zip::{CompressionType, ZipError, ZipFile, ZipFileEntry};

const CHECKPOINT_MAGIC: [u8; 4] = *b"MDL1";
const CHECKPOINT_VERSION: u16 = 1;
const BUFFER_SIZE: usize = 64 * 1024;
const REPLAY_BUFFER_SIZE: usize = 64 * 1024;
const INFLATE_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CompressionFormat {
    /// A normal zlib stream: RFC 1950 + RFC 1951.
    Zlib,

    /// A bare DEFLATE stream: RFC 1951.
    RawDeflate,
}

impl CompressionFormat {
    fn miniz_format(self) -> DataFormat {
        match self {
            Self::Zlib => DataFormat::Zlib,
            Self::RawDeflate => DataFormat::Raw,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadPaths {
    /// The final decompressed payload.
    pub output: PathBuf,

    /// A durable copy of every compressed byte already processed.
    ///
    /// This permits replay if a serialized miniz_oxide checkpoint becomes
    /// incompatible after a dependency upgrade.
    pub compressed_spool: PathBuf,

    /// The atomic checkpoint containing decoder state and offsets.
    pub checkpoint: PathBuf,
}

impl DownloadPaths {
    pub fn from_output(output: impl Into<PathBuf>) -> Self {
        let output = output.into();

        Self {
            compressed_spool: append_suffix(&output, ".compressed.part"),
            checkpoint: append_suffix(&output, ".checkpoint"),
            output,
        }
    }

    pub fn remove_partial_files(&self) -> io::Result<()> {
        remove_if_exists(&self.output)?;
        remove_if_exists(&self.compressed_spool)?;
        remove_if_exists(&self.checkpoint)?;
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: Url,
    pub compression: CompressionFormat,
    pub paths: DownloadPaths,

    /// A stable identity supplied by the caller, for example an expected
    /// SHA-256 digest of the final payload or a manifest asset ID.
    ///
    /// It prevents accidentally applying an old checkpoint to another URL.
    pub resource_id: [u8; 32],

    /// Number of compressed network bytes between checkpoint writes.
    pub checkpoint_interval: u64,
}

impl DownloadRequest {
    pub fn new(url: Url, compression: CompressionFormat, output: impl Into<PathBuf>) -> Self {
        let resource_id = sha256(url.as_str().as_bytes());

        Self {
            url,
            compression,
            paths: DownloadPaths::from_output(output),
            resource_id,
            checkpoint_interval: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub compressed_bytes: u64,
    pub decompressed_bytes: u64,
    pub resumed: bool,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("HTTP server returned {status} for {url}")]
    HttpStatus { status: StatusCode, url: Url },

    #[error("server returned Content-Encoding={encoding:?}; expected identity")]
    UnexpectedContentEncoding { encoding: Option<String> },

    #[error("server ignored the resume Range request")]
    RangeIgnored,

    #[error("server returned an invalid Content-Range header")]
    InvalidContentRange,

    #[error("checkpoint is malformed")]
    InvalidCheckpoint,

    #[error("checkpoint format version {found} is unsupported")]
    UnsupportedCheckpointVersion { found: u16 },

    #[error("checkpoint was created for a different URL or resource identity")]
    CheckpointResourceMismatch,

    #[error("checkpoint compression format does not match this request")]
    CheckpointCompressionMismatch,

    #[error("compressed spool is shorter than the checkpoint offset")]
    CompressedSpoolTooShort,

    #[error("decompressed output is shorter than the checkpoint offset")]
    DecompressedOutputTooShort,

    #[error("miniz_oxide stream error: {0:?}")]
    Inflate(MZError),

    #[error("inflate made no progress")]
    InflateNoProgress,

    #[error("received bytes after the end of the DEFLATE stream")]
    TrailingCompressedData,

    #[error("the HTTP response ended before the DEFLATE stream completed")]
    IncompleteDeflateStream,

    #[error("replayed stream produced {actual} bytes; checkpoint expected {expected}")]
    ReplayOutputMismatch { expected: u64, actual: u64 },

    #[error(transparent)]
    Zip(#[from] ZipError),

    #[error("server returned {0} for a ranged request")]
    UnexpectedStatus(StatusCode),

    #[error("entry `{0}` decompressed to an unexpected size")]
    SizeMismatch(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct Checkpoint {
    magic: [u8; 4],
    version: u16,

    resource_id: [u8; 32],
    compression: CompressionFormat,

    /// Bytes durably written to `compressed_spool`.
    compressed_offset: u64,

    /// Bytes durably written to `output`.
    decompressed_offset: u64,

    /// HTTP ETag captured when the checkpoint was made.
    ///
    /// On resume, this is sent in If-Range. If the representation changed,
    /// the server should return 200 instead of serving a stale byte range.
    etag: Option<String>,
}

impl Checkpoint {
    fn new(
        request: &DownloadRequest,
        compressed_offset: u64,
        decompressed_offset: u64,
        etag: Option<String>,
    ) -> Self {
        Self {
            magic: CHECKPOINT_MAGIC,
            version: CHECKPOINT_VERSION,
            resource_id: request.resource_id,
            compression: request.compression,
            compressed_offset,
            decompressed_offset,
            etag,
        }
    }

    fn validate(&self, request: &DownloadRequest) -> Result<(), DownloadError> {
        if self.magic != CHECKPOINT_MAGIC {
            return Err(DownloadError::InvalidCheckpoint);
        }

        if self.version != CHECKPOINT_VERSION {
            return Err(DownloadError::UnsupportedCheckpointVersion {
                found: self.version,
            });
        }

        if self.resource_id != request.resource_id {
            return Err(DownloadError::CheckpointResourceMismatch);
        }

        if self.compression != request.compression {
            return Err(DownloadError::CheckpointCompressionMismatch);
        }

        Ok(())
    }
}

struct ActiveDownload {
    inflate: Box<InflateState>,
    compressed_offset: u64,
    decompressed_offset: u64,
    etag: Option<String>,
    stream_finished: bool,
    resumed: bool,
}

impl ActiveDownload {
    fn fresh(format: CompressionFormat, etag: Option<String>) -> Self {
        Self {
            inflate: InflateState::new_boxed(format.miniz_format()),
            compressed_offset: 0,
            decompressed_offset: 0,
            etag,
            stream_finished: false,
            resumed: false,
        }
    }

    fn from_checkpoint(
        checkpoint: Checkpoint,
        request: &DownloadRequest,
    ) -> Result<Self, DownloadError> {
        // Rebuild a fresh decoder and fast-forward it by replaying the
        // already-downloaded compressed bytes from the spool file, since
        // InflateState cannot be persisted directly.
        let inflate = InflateState::new_boxed(request.compression.miniz_format());
        let mut spool = BufReader::new(File::open(&request.paths.compressed_spool)?);
        let sink = io::sink();
        let mut input = [0_u8; REPLAY_BUFFER_SIZE];
        let compressed_offset = 0_u64;
        let decompressed_offset = 0_u64;
        let stream_finished = false;

        while compressed_offset < checkpoint.compressed_offset {
            let count = spool.read(&mut input)?;
            if count == 0 {
                break;
            }
            let to_process = &input[..count];
        }

        if compressed_offset != checkpoint.compressed_offset
            || decompressed_offset != checkpoint.decompressed_offset
        {
            return Err(DownloadError::InvalidCheckpoint);
        }

        Ok(Self {
            inflate,
            compressed_offset,
            decompressed_offset,
            etag: checkpoint.etag,
            stream_finished,
            resumed: true,
        })
    }

    fn checkpoint(&self, request: &DownloadRequest) -> Checkpoint {
        Checkpoint::new(
            request,
            self.compressed_offset,
            self.decompressed_offset,
            self.etag.clone(),
        )
    }
}

/// A synchronous, resumable downloader.
///
/// The remote representation must be stable and range-addressable. This
/// downloader sends `Accept-Encoding: identity`; configure the origin so that
/// the payload itself is zlib or raw-DEFLATE if decompression is required.
pub struct Downloader {
    client: Client,
}

impl Downloader {
    pub fn new() -> Result<Self, DownloadError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .build()?;

        Ok(Self { client })
    }

    pub fn download(&self, request: &DownloadRequest) -> Result<DownloadResult, DownloadError> {
        ensure_parent_exists(&request.paths.output)?;
        ensure_parent_exists(&request.paths.compressed_spool)?;
        ensure_parent_exists(&request.paths.checkpoint)?;

        let mut active = self.restore_or_create(request)?;
        let response = self.open_response(request, &mut active)?;

        let mut spool = open_append(&request.paths.compressed_spool)?;
        let mut output = open_append(&request.paths.output)?;

        self.consume_response(request, &mut active, response, &mut spool, &mut output)?;

        spool.flush()?;
        spool.get_ref().sync_all()?;

        output.flush()?;
        output.get_ref().sync_all()?;

        if !active.stream_finished {
            self.write_checkpoint(request, &active)?;
            return Err(DownloadError::IncompleteDeflateStream);
        }

        remove_if_exists(&request.paths.checkpoint)?;
        remove_if_exists(&request.paths.compressed_spool)?;

        Ok(DownloadResult {
            compressed_bytes: active.compressed_offset,
            decompressed_bytes: active.decompressed_offset,
            resumed: active.resumed,
        })
    }

    fn restore_or_create(
        &self,
        request: &DownloadRequest,
    ) -> Result<ActiveDownload, DownloadError> {
        let checkpoint = match read_checkpoint(&request.paths.checkpoint) {
            Ok(checkpoint) => checkpoint,
            Err(DownloadError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                truncate_or_create(&request.paths.output)?;
                truncate_or_create(&request.paths.compressed_spool)?;
                return Ok(ActiveDownload::fresh(request.compression, None));
            }
            Err(error) => {
                return self.replay_or_restart(request, Some(error));
            }
        };

        if checkpoint.validate(request).is_err() {
            return self.replay_or_restart(request, None);
        }

        if verify_file_len(
            &request.paths.compressed_spool,
            checkpoint.compressed_offset,
        )
        .is_err()
            || verify_file_len(&request.paths.output, checkpoint.decompressed_offset).is_err()
        {
            return self.replay_or_restart(request, None);
        }

        ActiveDownload::from_checkpoint(checkpoint, request)
    }

    fn replay_or_restart(
        &self,
        request: &DownloadRequest,
        decode_error: Option<DownloadError>,
    ) -> Result<ActiveDownload, DownloadError> {
        let spool_len = match fs::metadata(&request.paths.compressed_spool) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                truncate_or_create(&request.paths.output)?;
                truncate_or_create(&request.paths.compressed_spool)?;
                remove_if_exists(&request.paths.checkpoint)?;
                return Ok(ActiveDownload::fresh(request.compression, None));
            }
            Err(error) => return Err(DownloadError::Io(error)),
        };

        if spool_len == 0 {
            truncate_or_create(&request.paths.output)?;
            remove_if_exists(&request.paths.checkpoint)?;
            return Ok(ActiveDownload::fresh(request.compression, None));
        }

        let mut active = ActiveDownload::fresh(request.compression, None);
        let mut spool = BufReader::new(File::open(&request.paths.compressed_spool)?);
        let mut sink = io::sink();
        let mut input = [0_u8; REPLAY_BUFFER_SIZE];

        loop {
            let count = spool.read(&mut input)?;

            if count == 0 {
                break;
            }

            self.inflate_bytes(&mut active, &input[..count], &mut sink)?;
        }

        let actual_output_len = fs::metadata(&request.paths.output)?.len();

        if actual_output_len != active.decompressed_offset {
            if let Some(original_error) = decode_error {
                return Err(original_error);
            }

            return Err(DownloadError::ReplayOutputMismatch {
                expected: actual_output_len,
                actual: active.decompressed_offset,
            });
        }

        active.resumed = true;
        self.write_checkpoint(request, &active)?;
        Ok(active)
    }

    fn open_response(
        &self,
        request: &DownloadRequest,
        active: &mut ActiveDownload,
    ) -> Result<Response, DownloadError> {
        let mut builder = self
            .client
            .get(request.url.clone())
            .header(ACCEPT_ENCODING, "identity");

        if active.compressed_offset != 0 {
            builder = builder.header(RANGE, format!("bytes={}-", active.compressed_offset));

            if let Some(etag) = &active.etag {
                builder = builder.header(IF_RANGE, etag);
            }
        }

        let response = builder.send()?;
        let status = response.status();

        self.ensure_identity_encoding(&response)?;

        match (active.compressed_offset, status) {
            (0, StatusCode::OK) => {
                active.etag = response_etag(&response);
                Ok(response)
            }

            (offset, StatusCode::PARTIAL_CONTENT) if offset != 0 => {
                validate_content_range(&response, offset)?;
                Ok(response)
            }

            (offset, StatusCode::OK) if offset != 0 => {
                // If If-Range failed, or the server ignores range requests,
                // its full response cannot be appended to our old stream.
                Err(DownloadError::RangeIgnored)
            }

            (_, status) => Err(DownloadError::HttpStatus {
                status,
                url: request.url.clone(),
            }),
        }
    }

    fn consume_response(
        &self,
        request: &DownloadRequest,
        active: &mut ActiveDownload,
        mut response: Response,
        spool: &mut BufWriter<File>,
        output: &mut BufWriter<File>,
    ) -> Result<(), DownloadError> {
        let mut input = [0_u8; BUFFER_SIZE];
        let mut checkpoint_after = active
            .compressed_offset
            .saturating_add(request.checkpoint_interval.max(1));

        loop {
            let read = response.read(&mut input)?;

            if read == 0 {
                break;
            }

            // Transaction order:
            // 1. Make source bytes durable in the replay spool.
            // 2. Advance the safe inflater state.
            // 3. Make decoded output durable.
            // 4. Atomically replace the state checkpoint.
            spool.write_all(&input[..read])?;

            self.inflate_bytes(active, &input[..read], output)?;

            if active.compressed_offset >= checkpoint_after {
                spool.flush()?;
                spool.get_ref().sync_data()?;

                output.flush()?;
                output.get_ref().sync_data()?;

                self.write_checkpoint(request, active)?;

                checkpoint_after = active
                    .compressed_offset
                    .saturating_add(request.checkpoint_interval.max(1));
            }
        }

        Ok(())
    }

    fn inflate_bytes(
        &self,
        active: &mut ActiveDownload,
        mut source: &[u8],
        output: &mut impl Write,
    ) -> Result<(), DownloadError> {
        let mut destination = [0_u8; BUFFER_SIZE];

        while !source.is_empty() {
            if active.stream_finished {
                return Err(DownloadError::TrailingCompressedData);
            }

            let result = inflate(&mut active.inflate, source, &mut destination, MZFlush::None);

            let consumed = result.bytes_consumed;
            let written = result.bytes_written;

            if written != 0 {
                output.write_all(&destination[..written])?;
                active.decompressed_offset += written as u64;
            }

            active.compressed_offset += consumed as u64;
            source = &source[consumed..];

            match result.status {
                Ok(MZStatus::Ok) => {
                    if consumed == 0 && written == 0 {
                        return Err(DownloadError::InflateNoProgress);
                    }
                }

                Ok(MZStatus::StreamEnd) => {
                    active.stream_finished = true;

                    if !source.is_empty() {
                        return Err(DownloadError::TrailingCompressedData);
                    }
                }

                Ok(other) => {
                    return Err(DownloadError::Inflate(MZError::Stream));
                }

                Err(error) => return Err(DownloadError::Inflate(error)),
            }
        }

        Ok(())
    }

    fn ensure_identity_encoding(&self, response: &Response) -> Result<(), DownloadError> {
        let encoding = response
            .headers()
            .get(CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        match encoding.as_deref() {
            None | Some("identity") => Ok(()),
            _ => Err(DownloadError::UnexpectedContentEncoding { encoding }),
        }
    }

    fn write_checkpoint(
        &self,
        request: &DownloadRequest,
        active: &ActiveDownload,
    ) -> Result<(), DownloadError> {
        let checkpoint = active.checkpoint(request);

        let bytes =
            postcard::to_allocvec(&checkpoint).map_err(|_| DownloadError::InvalidCheckpoint)?;

        atomic_write(&request.paths.checkpoint, &bytes)?;
        Ok(())
    }
}

pub type ProgressCallback = Arc<dyn Fn(usize) + Send + Sync>;

/// Reads a remote ZIP's central directory once via HTTP Range requests
/// (see `ZipFile::fetch`), then allows downloading individual entries by
/// range without fetching the whole archive.
pub struct ZipDownloader {
    offer_id: String,
    url: String,
    path: PathBuf,
    client: Client,
    zip: ZipFile,
}

impl ZipDownloader {
    pub async fn new(offer_id: &str, url: &str, path: &Path) -> Result<Self, DownloadError> {
        let zip = ZipFile::fetch(url).await?;

        Ok(Self {
            offer_id: offer_id.to_owned(),
            url: url.to_owned(),
            path: path.to_owned(),
            client: Client::new(),
            zip,
        })
    }

    pub fn manifest(&self) -> &ZipFile {
        &self.zip
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn offer_id(&self) -> &str {
        &self.offer_id
    }

    /// Downloads and, if needed, decompresses a single central-directory
    /// entry using an HTTP Range request scoped to `[data_offset,
    /// data_offset + compressed_size)`.
    pub async fn download_single_file(
        &self,
        entry: &ZipFileEntry,
        progress: Option<ProgressCallback>,
    ) -> Result<(), DownloadError> {
        let start = *entry.data_offset();
        let end = start + *entry.compressed_size() - 1;

        let response = self
            .client
            .get(&self.url)
            .header("range", format!("bytes={start}-{end}"))
            .send()?;

        if !response.status().is_success() {
            return Err(DownloadError::UnexpectedStatus(response.status()));
        }

        let dest_path = self.path.join(entry.name());
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut out = File::create(&dest_path)?;

        match entry.compression_type() {
            CompressionType::None => {
                self.write_stored(response, &mut out, progress)?;
            }
            CompressionType::Deflate => {
                self.write_deflated(response, &mut out, entry, progress)?;
            }
        }

        out.flush()?;
        Ok(())
    }

    fn write_stored(
        &self,
        mut response: Response,
        out: &mut File,
        progress: Option<ProgressCallback>,
    ) -> Result<(), DownloadError> {
        let mut buffer = [0_u8; BUFFER_SIZE];
        loop {
            let read = response.read(&mut buffer)?;

            if read == 0 {
                break;
            }

            out.write_all(&buffer[..read])?;

            if let Some(cb) = &progress {
                cb(read);
            }
        }

        Ok(())
    }

    fn write_deflated(
        &self,
        mut response: Response,
        out: &mut File,
        entry: &ZipFileEntry,
        progress: Option<ProgressCallback>,
    ) -> Result<(), DownloadError> {
        let mut inflate_state = InflateState::new_boxed(DataFormat::Raw);
        let mut written_total: i64 = 0;
        let mut input = [0_u8; BUFFER_SIZE];

        loop {
            let read = response.read(&mut input)?;

            if read == 0 {
                break;
            }

            let compressed_len = read;
            let mut source: &[u8] = &input[..read];
            let mut destination = [0_u8; INFLATE_CHUNK];

            while !source.is_empty() {
                let result = inflate(&mut inflate_state, source, &mut destination, MZFlush::None);

                let consumed = result.bytes_consumed;
                let produced = result.bytes_written;

                if produced != 0 {
                    out.write_all(&destination[..produced])?;
                    written_total += produced as i64;
                }

                source = &source[consumed..];

                match result.status {
                    Ok(MZStatus::Ok) => {
                        if consumed == 0 && produced == 0 {
                            return Err(DownloadError::InflateNoProgress);
                        }
                    }
                    Ok(MZStatus::StreamEnd) => break,
                    Ok(_) => {}
                    Err(err) => return Err(DownloadError::Inflate(err)),
                }
            }

            if let Some(cb) = &progress {
                cb(compressed_len);
            }
        }

        if written_total != *entry.uncompressed_size() {
            return Err(DownloadError::SizeMismatch(entry.name().clone()));
        }

        Ok(())
    }
}

fn read_checkpoint(path: &Path) -> Result<Checkpoint, DownloadError> {
    let bytes = fs::read(path)?;

    postcard::from_bytes::<Checkpoint>(&bytes).map_err(|_| DownloadError::InvalidCheckpoint)
}

fn validate_content_range(response: &Response, expected_start: u64) -> Result<(), DownloadError> {
    let value = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or(DownloadError::InvalidContentRange)?;

    let value = value
        .strip_prefix("bytes ")
        .ok_or(DownloadError::InvalidContentRange)?;

    let (range, _) = value
        .split_once('/')
        .ok_or(DownloadError::InvalidContentRange)?;

    let (start, _) = range
        .split_once('-')
        .ok_or(DownloadError::InvalidContentRange)?;

    let start = start
        .parse::<u64>()
        .map_err(|_| DownloadError::InvalidContentRange)?;

    if start != expected_start {
        return Err(DownloadError::InvalidContentRange);
    }

    Ok(())
}

fn response_etag(response: &Response) -> Option<String> {
    response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn open_append(path: &Path) -> Result<BufWriter<File>, DownloadError> {
    Ok(BufWriter::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)?,
    ))
}

fn truncate_or_create(path: &Path) -> Result<(), DownloadError> {
    File::create(path)?.sync_all()?;
    Ok(())
}

fn verify_file_len(path: &Path, expected_minimum: u64) -> Result<(), DownloadError> {
    let actual = fs::metadata(path)?.len();

    if actual < expected_minimum {
        return Err(DownloadError::CompressedSpoolTooShort);
    }

    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), DownloadError> {
    let temporary = append_suffix(path, ".tmp");

    {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    #[cfg(windows)]
    {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }

    fs::rename(temporary, path)?;
    Ok(())
}

fn ensure_parent_exists(path: &Path) -> Result<(), DownloadError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
