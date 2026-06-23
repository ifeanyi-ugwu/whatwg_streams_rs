//! Streaming gzip decompression as a `TransformStream`.
//!
//! Compressed bytes are read from a file in fixed-size chunks and fed to a stateful
//! `Decompress`. One input chunk can expand into more output than a single buffer holds,
//! so `transform` loops — tracking how much the inflater consumed and produced each call
//! — and emits every decompressed batch downstream. Pair it with `gzip_transform`, which
//! writes `compressed_data.gz`.
//!
//! Run with: `cargo run --example decompress_stream`

use flate2::write::GzDecoder;
use std::io::{self, Write};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Reads a file in fixed-size chunks, then closes.
pub struct CompressedFileSource {
    file: File,
    buffer_size: usize,
}

impl CompressedFileSource {
    pub async fn new(path: &str, buffer_size: usize) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path).await?,
            buffer_size,
        })
    }
}

impl ReadableSource<Vec<u8>> for CompressedFileSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let mut buf = vec![0u8; self.buffer_size];
        let n = self.file.read(&mut buf).await?;
        if n == 0 {
            controller.close()?;
        } else {
            buf.truncate(n);
            controller.enqueue(buf)?;
        }
        Ok(())
    }
}

/// Inflates gzip data incrementally, emitting each decompressed batch.
///
/// `write::GzDecoder` decompresses gzip (header, payload, and CRC footer) as bytes are
/// written to it, appending plaintext to an inner `Vec` that is drained after each chunk.
pub struct GzipDecompressor {
    decoder: GzDecoder<Vec<u8>>,
}

impl Default for GzipDecompressor {
    fn default() -> Self {
        Self {
            decoder: GzDecoder::new(Vec::new()),
        }
    }
}

impl Transformer<Vec<u8>, Vec<u8>> for GzipDecompressor {
    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.decoder.write_all(&chunk)?;
        let produced = std::mem::take(self.decoder.get_mut());
        if !produced.is_empty() {
            controller.enqueue(produced)?;
        }
        Ok(())
    }

    async fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        // Finalize the gzip stream (verifies the CRC) and emit any trailing plaintext.
        self.decoder.try_finish()?;
        let produced = std::mem::take(self.decoder.get_mut());
        if !produced.is_empty() {
            controller.enqueue(produced)?;
        }
        Ok(())
    }
}

/// Writes decompressed bytes to a file.
pub struct DecompressedFileWriter {
    file_path: String,
    file: Option<File>,
}

impl DecompressedFileWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file: None,
        }
    }
}

impl WritableSink<Vec<u8>> for DecompressedFileWriter {
    async fn start(&mut self, _c: &mut WritableStreamDefaultController) -> StreamResult<()> {
        self.file = Some(File::create(&self.file_path).await?);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _c: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file {
            file.write_all(&chunk).await?;
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(file) = self.file.take() {
            file.sync_all().await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let source_path = "compressed_data.gz";
    let dest_path = "decompressed_output.txt";

    let input = CompressedFileSource::new(source_path, 1024)
        .await
        .expect("open compressed_data.gz (run the gzip_transform example first)");
    let source = ReadableStream::builder(input).spawn(tokio::spawn);
    let inflate = TransformStream::builder(GzipDecompressor::default()).spawn(tokio::spawn);
    let sink = WritableStream::builder(DecompressedFileWriter::new(dest_path)).spawn(tokio::spawn);

    let plain = source.pipe_through(inflate, None).spawn(tokio::spawn);
    plain
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");

    let size = tokio::fs::metadata(dest_path)
        .await
        .expect("stat output")
        .len();
    println!("wrote {dest_path} ({size} bytes)");
}
