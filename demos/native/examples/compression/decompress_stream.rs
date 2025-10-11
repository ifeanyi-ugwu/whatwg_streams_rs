//! Decompression Transform Example
//!
//! Demonstrates streaming decompression using a transform stream that:
//! 1. Reads compressed data chunks from a file
//! 2. Decompresses each chunk using gzip
//! 3. Outputs decompressed data to a file
//!
//! This example shows:
//! - How transforms can reverse gzip compression
//! - Chunked streaming decompression with flush
//! - Using readable → transform → writable pipeline

use flate2::Decompress;
use flate2::FlushDecompress;
use std::io;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    transform::{TransformStream, TransformStreamDefaultController, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A readable source that streams compressed data from a file
pub struct CompressedFileSource {
    file: File,
    buffer_size: usize,
}

impl CompressedFileSource {
    pub async fn new(path: &str, buffer_size: usize) -> io::Result<Self> {
        let file = File::open(path).await?;
        Ok(Self { file, buffer_size })
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

/// A transform stream that decompresses gzip data
pub struct GzipDecompressor {
    decompressor: Decompress,
}

impl GzipDecompressor {
    pub fn new() -> Self {
        Self {
            decompressor: Decompress::new(true), // true = gzip header
        }
    }
}

impl Transformer<Vec<u8>, Vec<u8>> for GzipDecompressor {
    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let mut output = vec![0u8; chunk.len() * 5]; // oversize buffer
        let mut total_out = 0;

        let status = self
            .decompressor
            .decompress(&chunk, &mut output, FlushDecompress::None)
            .map_err(|e| format!("Decompression error: {}", e))?;

        total_out = self.decompressor.total_out() as usize;

        if total_out > 0 {
            output.truncate(total_out);
            controller.enqueue(output)?;
        }

        println!("🔄 Decompressed chunk: {:?} ({:?})", total_out, status);
        Ok(())
    }

    async fn flush(
        &mut self,
        _controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        println!("✅ Decompression complete");
        Ok(())
    }
}

/// A writable sink that writes decompressed text to a file
pub struct DecompressedFileWriter {
    file_path: String,
    file_handle: Option<File>,
}

impl DecompressedFileWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file_handle: None,
        }
    }
}

impl WritableSink<Vec<u8>> for DecompressedFileWriter {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.file_handle = Some(File::create(&self.file_path).await?);
        println!("📂 Created decompressed output file: {}", self.file_path);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file_handle {
            file.write_all(&chunk).await?;
            println!("💾 Wrote {} bytes of decompressed data", chunk.len());
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(file) = self.file_handle.take() {
            file.sync_all().await?;
            println!("🔒 Decompressed file closed");
        }
        Ok(())
    }
}

/// Example function demonstrating decompression
pub async fn run_decompression_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Decompression Stream Example ===");

    // Source compressed file (produced by gzip example)
    let source_path = "compressed_data.gz";
    let dest_path = "decompressed_output.txt";

    let readable =
        ReadableStream::new_default(CompressedFileSource::new(source_path, 1024).await?, None);
    let transform = TransformStream::new(GzipDecompressor::new());
    let writable = WritableStream::new_with_spawn(
        DecompressedFileWriter::new(dest_path),
        Box::new(CountQueuingStrategy::new(3)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    readable
        .pipe_through(transform, None)
        .pipe_to(&writable, None)
        .await?;

    println!("✅ Decompression pipeline finished");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_decompression_example().await
}
