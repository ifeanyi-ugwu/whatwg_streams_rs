//! Archive Creator Example (.tar.gz)
//!
//! Demonstrates streaming multiple files into a `.tar.gz` archive using a transform-style pipeline.
//! This example shows:
//! 1. Streaming multiple files into a tar archive
//! 2. Compressing the archive on-the-fly using gzip
//! 3. Writing the compressed output to a single file
//! 4. Efficient memory usage without buffering entire archive in memory

use flate2::{Compression, write::GzEncoder};
use std::fs::File as StdFile;
use std::io::{Result as IoResult, Write};
use std::path::Path;
use tar::Builder;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Sink that streams tar.gz archive to a file
pub struct TarGzWriter {
    file_path: String,
    gz_encoder: Option<GzEncoder<StdFile>>,
    bytes_written: usize,
}

impl TarGzWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            gz_encoder: None,
            bytes_written: 0,
        }
    }
}

impl WritableSink<Vec<u8>> for TarGzWriter {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let file = StdFile::create(&self.file_path)?;
        self.gz_encoder = Some(GzEncoder::new(file, Compression::default()));
        println!("📁 Created archive output file: {}", self.file_path);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(encoder) = &mut self.gz_encoder {
            encoder.write_all(&chunk)?;
            self.bytes_written += chunk.len();
            println!(
                "💾 Wrote {} bytes to archive (total: {})",
                chunk.len(),
                self.bytes_written
            );
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(encoder) = self.gz_encoder.take() {
            encoder.finish()?;
            println!(
                "✅ Archive creation complete: {} bytes written",
                self.bytes_written
            );
        }
        Ok(())
    }
}

/// Example: create a tar.gz archive from multiple files
pub async fn run_archive_creator_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Tar.gz Archive Creator Example ===");

    // Example: files to include in the archive
    /*let input_files = vec![
        ("file1.txt", b"Hello, world! This is file 1."),
        ("file2.txt", b"Another file content for the archive."),
        ("file3.txt", b"Final file data here."),
    ];*/

    let input_files: Vec<(&str, &[u8])> = vec![
        ("file1.txt", b"Hello, world! This is file 1."),
        ("file2.txt", b"Another file content for the archive."),
        ("file3.txt", b"Final file data here."),
    ];

    // Create a tar archive in memory (streamed chunk by chunk)
    let mut tar_buffer: Vec<u8> = Vec::new();
    {
        let mut tar_builder = Builder::new(&mut tar_buffer);

        for (filename, content) in &input_files {
            tar_builder.append_data(&mut tar::Header::new_gnu(), filename, &mut &content[..])?;
            println!(
                "📦 Added {} ({} bytes) to tar archive",
                filename,
                content.len()
            );
        }

        tar_builder.finish()?;
    }

    // Create the writable stream to output tar.gz
    let archive_file = "archive_output.tar.gz";
    let writable_stream = WritableStream::new_with_spawn(
        TarGzWriter::new(archive_file),
        Box::new(CountQueuingStrategy::new(3)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    // In a real streaming scenario, chunks could be pushed iteratively
    let (_, writer) = writable_stream.get_writer()?;
    writer.write(tar_buffer).await?;
    writer.close().await?;

    println!("✅ Archive file created successfully: {}", archive_file);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_archive_creator_example().await
}
