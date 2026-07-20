use async_std::fs::File as AsyncFile;
use blocking::Unblock;
use futures::io::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use crate::ImageError;

const DECODE_PIPE_CAPACITY: usize = 1024 * 1024;
const INSPECTION_BUFFER_SIZE: usize = 1024 * 1024;
const ZSTD_MAGIC: u32 = 0xfd2f_b528;
const ZSTD_SKIPPABLE_MAGIC: u32 = 0x184d_2a50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFormat {
    Raw,
    Zstandard,
}

/// A file reader that transparently expands supported image containers.
///
/// Zstandard input is decoded on a blocking worker and exposed as an async
/// stream. The expanded image is never stored in memory or in a temporary
/// file.
pub struct ImageReader {
    inner: ImageReaderInner,
    compressed_size: u64,
}

enum ImageReaderInner {
    Raw(AsyncFile),
    Zstandard(Option<AsyncZstdDecoder>),
}

type ZstdDecoder = zstd::stream::read::Decoder<'static, std::io::BufReader<File>>;
type AsyncZstdDecoder = Unblock<NonEmptyRead<ZstdDecoder>>;

impl ImageReader {
    /// Inspect an already-open image and select the appropriate streaming
    /// reader. Format detection uses frame magic rather than the filename.
    pub fn new(mut file: File) -> Result<Self, ImageError> {
        let metadata = file.metadata().map_err(|why| ImageError::Metadata { why })?;
        if !metadata.is_file() {
            return Err(ImageError::NotAFile);
        }

        let compressed_size = metadata.len();
        let format = detect_image_format(&mut file)?;
        let inner = match format {
            ImageFormat::Raw => ImageReaderInner::Raw(file.into()),
            ImageFormat::Zstandard => ImageReaderInner::Zstandard(Some(
                create_zstd_decoder(file).map_err(|why| ImageError::InvalidZstandard { why })?,
            )),
        };

        Ok(Self { inner, compressed_size })
    }

    pub fn format(&self) -> ImageFormat {
        match self.inner {
            ImageReaderInner::Raw(_) => ImageFormat::Raw,
            ImageReaderInner::Zstandard(_) => ImageFormat::Zstandard,
        }
    }

    /// Fully read and validate the source, returning its exact expanded size.
    /// The reader is rewound before this method returns.
    pub async fn inspect(&mut self) -> Result<u64, ImageError> {
        self.rewind().await.map_err(|why| ImageError::ReadError { why })?;

        let format = self.format();
        let result = match format {
            ImageFormat::Raw => Ok(self.compressed_size),
            ImageFormat::Zstandard => self.inspect_zstandard().await,
        };

        // Recreate the decoder even after a decode failure. This keeps callers
        // from accidentally observing an arbitrary point in the source.
        let rewind_result = self.rewind().await;
        let expanded_size = result?;
        rewind_result.map_err(|why| ImageError::ReadError { why })?;

        if expanded_size == 0 {
            return Err(ImageError::Empty);
        }

        Ok(expanded_size)
    }

    async fn inspect_zstandard(&mut self) -> Result<u64, ImageError> {
        let mut buffer = vec![0u8; INSPECTION_BUFFER_SIZE];
        let mut expanded_size = 0u64;

        loop {
            let read =
                self.read(&mut buffer).await.map_err(|why| ImageError::InvalidZstandard { why })?;
            if read == 0 {
                return Ok(expanded_size);
            }

            expanded_size =
                expanded_size.checked_add(read as u64).ok_or(ImageError::ExpandedSizeOverflow)?;
        }
    }

    /// Reset the expanded byte stream to its beginning.
    pub async fn rewind(&mut self) -> io::Result<()> {
        match &mut self.inner {
            ImageReaderInner::Raw(file) => {
                file.seek(SeekFrom::Start(0)).await?;
            }
            ImageReaderInner::Zstandard(decoder) => {
                if let Some(active) = decoder.take() {
                    // Wait for the blocking worker to relinquish the decoder
                    // before replacing it.
                    let mut source = active.into_inner().await.0.finish().into_inner();
                    source.seek(SeekFrom::Start(0))?;
                    *decoder = Some(create_zstd_decoder(source)?);
                }
            }
        }

        Ok(())
    }
}

impl AsyncRead for ImageReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.inner {
            ImageReaderInner::Raw(file) => Pin::new(file).poll_read(cx, buf),
            ImageReaderInner::Zstandard(decoder) => {
                Pin::new(decoder.as_mut().expect("missing Zstandard decoder")).poll_read(cx, buf)
            }
        }
    }
}

fn detect_image_format(image: &mut File) -> Result<ImageFormat, ImageError> {
    let mut prefix = [0u8; 4];
    let read = image.read_exact(&mut prefix);
    image.seek(SeekFrom::Start(0)).map_err(|why| ImageError::ReadError { why })?;
    match read {
        Ok(()) => (),
        Err(why) if why.kind() == io::ErrorKind::UnexpectedEof => return Ok(ImageFormat::Raw),
        Err(why) => return Err(ImageError::ReadError { why }),
    }

    let magic = u32::from_le_bytes(prefix);
    if magic == ZSTD_MAGIC || magic & 0xffff_fff0 == ZSTD_SKIPPABLE_MAGIC {
        Ok(ImageFormat::Zstandard)
    } else {
        Ok(ImageFormat::Raw)
    }
}

fn create_zstd_decoder(source: File) -> io::Result<AsyncZstdDecoder> {
    let decoder = zstd::stream::read::Decoder::new(source)?;
    Ok(Unblock::with_capacity(DECODE_PIPE_CAPACITY, NonEmptyRead(decoder)))
}

/// `blocking::Unblock` may issue an empty read when its pipe is exactly full.
/// Zstd interprets that as a full output buffer and eventually reports a false
/// no-progress error, so short-circuit it at the adapter boundary.
struct NonEmptyRead<R>(R);

impl<R: Read> Read for NonEmptyRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            Ok(0)
        } else {
            self.0.read(buf)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor;
    use std::{
        fs,
        io::Write,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn image_file(contents: &[u8]) -> File {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "popsicle-image-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, contents).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        fs::remove_file(path).unwrap();
        file
    }

    fn checked_frame(contents: &[u8]) -> Vec<u8> {
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
        encoder.include_checksum(true).unwrap();
        encoder.write_all(contents).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn raw_images_report_the_file_size_and_rewind() {
        executor::block_on(async {
            let mut reader = ImageReader::new(image_file(b"raw image bytes")).unwrap();
            assert_eq!(reader.inspect().await.unwrap(), 15);
            assert_eq!(reader.format(), ImageFormat::Raw);

            let mut first = [0u8; 3];
            reader.read_exact(&mut first).await.unwrap();
            assert_eq!(&first, b"raw");
            reader.rewind().await.unwrap();
            reader.read_exact(&mut first).await.unwrap();
            assert_eq!(&first, b"raw");
        });
    }

    #[test]
    fn concatenated_and_skippable_zstd_frames_stream_and_rewind() {
        let mut compressed = Vec::new();
        compressed.extend_from_slice(&0x184d_2a50u32.to_le_bytes());
        compressed.extend_from_slice(&3u32.to_le_bytes());
        compressed.extend_from_slice(b"tag");
        compressed.extend_from_slice(&checked_frame(b"first "));
        compressed.extend_from_slice(&checked_frame(b"second"));

        executor::block_on(async {
            let mut reader = ImageReader::new(image_file(&compressed)).unwrap();
            assert_eq!(reader.inspect().await.unwrap(), 12);
            assert_eq!(reader.format(), ImageFormat::Zstandard);

            let mut decoded = Vec::new();
            reader.read_to_end(&mut decoded).await.unwrap();
            assert_eq!(decoded, b"first second");
            reader.rewind().await.unwrap();
            decoded.clear();
            reader.read_to_end(&mut decoded).await.unwrap();
            assert_eq!(decoded, b"first second");
        });
    }

    #[test]
    fn truncated_zstd_checksum_is_rejected() {
        let mut compressed = checked_frame(b"must validate the final checksum");
        compressed.pop();

        let why = executor::block_on(async {
            let mut reader = ImageReader::new(image_file(&compressed)).unwrap();
            reader.inspect().await.unwrap_err()
        });
        assert!(matches!(why, ImageError::InvalidZstandard { .. }));
    }

    #[test]
    fn trailing_garbage_is_rejected() {
        let mut compressed = checked_frame(b"payload");
        compressed.extend_from_slice(b"not another frame");

        let why = executor::block_on(async {
            let mut reader = ImageReader::new(image_file(&compressed)).unwrap();
            reader.inspect().await.unwrap_err()
        });
        assert!(matches!(why, ImageError::InvalidZstandard { .. }));
    }

    #[test]
    fn decoded_empty_stream_is_rejected() {
        let compressed = checked_frame(b"");
        let why = executor::block_on(async {
            let mut reader = ImageReader::new(image_file(&compressed)).unwrap();
            reader.inspect().await.unwrap_err()
        });
        assert!(matches!(why, ImageError::Empty));
    }
}
