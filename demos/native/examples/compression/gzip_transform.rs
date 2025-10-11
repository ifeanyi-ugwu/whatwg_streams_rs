//! Gzip Compression Transform Example
//!
//! Demonstrates streaming compression using a transform stream that:
//! 1. Reads data chunks from a source
//! 2. Compresses each chunk using gzip
//! 3. Outputs compressed data to a file
//!
//! This example shows:
//! - How transforms work with binary data (Vec<u8>)
//! - Stateful transformers that maintain compression context
//! - Proper resource cleanup in flush() method
//! - Why BYOB isn't needed - compression operates on data chunks, not buffer management

use flate2::{Compression, write::GzEncoder};
use std::io::Write;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    transform::{TransformStream, TransformStreamDefaultController, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A readable source that generates text data in chunks
/// Simulates reading a large text file or streaming text data
pub struct TextDataSource {
    chunks: Vec<String>,
    index: usize,
}

impl TextDataSource {
    pub fn new(text_chunks: Vec<String>) -> Self {
        Self {
            chunks: text_chunks,
            index: 0,
        }
    }

    /// Create a source with sample data for demonstration
    pub fn sample_data() -> Self {
        let chunks = vec![
            "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(100),
            "Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ".repeat(100),
            "Ut enim ad minim veniam, quis nostrud exercitation ullamco. ".repeat(100),
            "Laboris nisi ut aliquip ex ea commodo consequat. ".repeat(100),
            "Duis aute irure dolor in reprehenderit in voluptate velit esse. ".repeat(100),
        ];
        Self::new(chunks)
    }
}

impl ReadableSource<Vec<u8>> for TextDataSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if self.index < self.chunks.len() {
            let chunk = self.chunks[self.index].as_bytes().to_vec();
            self.index += 1;
            controller.enqueue(chunk)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// A transform stream that compresses data using gzip
/// Note: This operates on Vec<u8> chunks, not individual buffers
pub struct GzipCompressor {
    encoder: Option<GzEncoder<Vec<u8>>>,
    total_input: usize,
    total_output: usize,
}

impl GzipCompressor {
    pub fn new() -> Self {
        Self {
            encoder: None,
            total_input: 0,
            total_output: 0,
        }
    }
}

impl Transformer<Vec<u8>, Vec<u8>> for GzipCompressor {
    async fn start(
        &mut self,
        _controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        // Initialize the gzip encoder
        self.encoder = Some(GzEncoder::new(Vec::new(), Compression::default()));
        println!("🗜️  Gzip compressor initialized");
        Ok(())
    }

    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if let Some(encoder) = &mut self.encoder {
            let input_size = chunk.len();
            self.total_input += input_size;

            // Write the chunk to the compressor
            encoder
                .write_all(&chunk)
                .map_err(|e| format!("Compression error: {}", e))?;

            // For streaming compression, we need to flush periodically to get output
            encoder
                .flush()
                .map_err(|e| format!("Compression flush error: {}", e))?;

            // Get any compressed data that's ready
            let compressed_data = encoder.get_ref().clone();
            if compressed_data.len() > self.total_output {
                let new_data = compressed_data[self.total_output..].to_vec();
                self.total_output = compressed_data.len();

                if !new_data.is_empty() {
                    controller.enqueue(new_data)?;
                }
            }

            println!(
                "🔄 Compressed {} bytes input, {} bytes output so far",
                input_size, self.total_output
            );
        }
        Ok(())
    }

    async fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if let Some(encoder) = self.encoder.take() {
            // Finish compression and get final data
            let final_compressed = encoder
                .finish()
                .map_err(|e| format!("Final compression error: {}", e))?;

            // Send any remaining compressed data
            if final_compressed.len() > self.total_output {
                let final_chunk = final_compressed[self.total_output..].to_vec();
                if !final_chunk.is_empty() {
                    controller.enqueue(final_chunk)?;
                }
            }

            let compression_ratio = if self.total_input > 0 {
                (final_compressed.len() as f64 / self.total_input as f64) * 100.0
            } else {
                0.0
            };

            println!("✅ Compression complete!");
            println!(
                "📊 Input: {} bytes, Output: {} bytes",
                self.total_input,
                final_compressed.len()
            );
            println!("📈 Compression ratio: {:.1}%", compression_ratio);
        }
        Ok(())
    }
}

/// A writable sink that saves compressed data to a file
pub struct CompressedFileWriter {
    file_path: String,
    file_handle: Option<File>,
    bytes_written: usize,
}

impl CompressedFileWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file_handle: None,
            bytes_written: 0,
        }
    }
}

impl WritableSink<Vec<u8>> for CompressedFileWriter {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.file_handle = Some(File::create(&self.file_path).await?);
        println!("📁 Created compressed output file: {}", self.file_path);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file_handle {
            file.write_all(&chunk).await?;
            self.bytes_written += chunk.len();
            println!(
                "💾 Wrote {} bytes to file (total: {})",
                chunk.len(),
                self.bytes_written
            );
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(file) = self.file_handle.take() {
            file.sync_all().await?;
            println!(
                "🔒 Compressed file closed: {} bytes written",
                self.bytes_written
            );
        }
        Ok(())
    }
}

/// Example function demonstrating streaming gzip compression
pub async fn run_gzip_compression_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Gzip Compression Transform Example ===");

    // Create a readable stream with sample text data
    let readable_stream = ReadableStream::new_default(TextDataSource::sample_data(), None);

    // Create the gzip compression transform
    let gzip_transform = TransformStream::new(GzipCompressor::new());

    // Create writable stream for compressed output
    let output_file = "compressed_data.gz";
    let writable_stream = WritableStream::new_with_spawn(
        CompressedFileWriter::new(output_file),
        Box::new(CountQueuingStrategy::new(3)), // Small buffer for demo
        |fut| {
            tokio::spawn(fut);
        },
    );

    println!("🚀 Starting compression pipeline...");

    // Execute the pipeline: Text Data -> Gzip Transform -> Compressed File
    readable_stream
        .pipe_through(gzip_transform, None)
        .pipe_to(&writable_stream, None)
        .await?;

    println!("✅ Compression pipeline completed!");

    // Verify the compressed file was created
    match tokio::fs::metadata(output_file).await {
        Ok(metadata) => {
            println!("📁 Compressed file size: {} bytes", metadata.len());

            // Test decompression to verify integrity
            test_decompression(output_file).await?;
        }
        Err(e) => println!("⚠️  Could not read compressed file metadata: {}", e),
    }

    Ok(())
}

/// Helper function to test that the compressed data can be decompressed
async fn test_decompression(compressed_file: &str) -> Result<(), Box<dyn std::error::Error>> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    println!("🔍 Testing decompression...");

    let compressed_data = tokio::fs::read(compressed_file).await?;
    let mut decoder = GzDecoder::new(&compressed_data[..]);
    let mut decompressed = String::new();
    decoder.read_to_string(&mut decompressed)?;

    println!("✅ Decompression successful!");
    println!("📄 Decompressed {} characters", decompressed.len());

    // Show a snippet of the decompressed data
    let snippet = if decompressed.len() > 100 {
        format!("{}...", &decompressed[..100])
    } else {
        decompressed
    };
    println!("📖 Sample: {}", snippet);

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_gzip_compression_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read;
    use tokio::fs;

    #[tokio::test]
    async fn test_gzip_compression_pipeline() {
        let test_data = vec![
            "Hello, world! ".repeat(50),
            "This is test data. ".repeat(50),
            "Final chunk of data. ".repeat(50),
        ];

        let readable = ReadableStream::new_default(TextDataSource::new(test_data), None);
        let transform = TransformStream::new(GzipCompressor::new());
        let output_path = "test_compressed.gz";
        let writable = WritableStream::new_with_spawn(
            CompressedFileWriter::new(output_path),
            Box::new(CountQueuingStrategy::new(5)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        // Run compression pipeline
        readable
            .pipe_through(transform, None)
            .pipe_to(&writable, None)
            .await
            .expect("Compression pipeline should complete");

        // Verify the file was created and can be decompressed
        let compressed_data = fs::read(output_path).await.unwrap();
        assert!(!compressed_data.is_empty());

        // Test decompression
        let mut decoder = GzDecoder::new(&compressed_data[..]);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();

        // Verify content is correct
        assert!(decompressed.contains("Hello, world!"));
        assert!(decompressed.contains("This is test data."));
        assert!(decompressed.contains("Final chunk of data."));

        // Clean up
        let _ = fs::remove_file(output_path).await;
    }

    #[tokio::test]
    async fn test_empty_compression() {
        let readable = ReadableStream::new_default(TextDataSource::new(vec![]), None);
        let transform = TransformStream::new(GzipCompressor::new());
        let output_path = "test_empty_compressed.gz";
        let writable = WritableStream::new_with_spawn(
            CompressedFileWriter::new(output_path),
            Box::new(CountQueuingStrategy::new(1)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        readable
            .pipe_through(transform, None)
            .pipe_to(&writable, None)
            .await
            .expect("Empty compression should complete");

        // Even empty compression should produce a valid gzip file
        let compressed_data = fs::read(output_path).await.unwrap();
        assert!(!compressed_data.is_empty()); // Gzip header + footer

        // Clean up
        let _ = fs::remove_file(output_path).await;
    }
}
