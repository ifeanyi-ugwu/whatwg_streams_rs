//! BYOB File Reader Example
//!
//! Demonstrates when and how to use BYOB (Bring Your Own Buffer) streams effectively.
//! This example shows:
//! 1. Reading large files with controlled buffer allocation
//! 2. Buffer reuse to minimize memory allocations
//! 3. When BYOB provides real performance benefits
//! 4. Proper error handling and resource cleanup
//!
//! BYOB is beneficial when:
//! - Reading large files where you want to control buffer sizes
//! - Network operations where you want to reuse buffers
//! - Performance-critical scenarios where allocation overhead matters
//! - You need specific buffer alignment or characteristics

use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{
        ReadableByteSource, ReadableByteStreamController,
        /* ReadableByteStream,*/ ReadableStream,
    },
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A BYOB source that reads from a large file efficiently
/// Uses the provided buffer to avoid allocations on each read
pub struct LargeFileByobSource {
    file: Option<File>,
    file_path: PathBuf,
    total_bytes_read: u64,
}

impl LargeFileByobSource {
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            file: None,
            file_path: path.into(),
            total_bytes_read: 0,
        }
    }
}

impl ReadableByteSource for LargeFileByobSource {
    async fn start(&mut self, _controller: &mut ReadableByteStreamController) -> StreamResult<()> {
        println!("📂 Opening file: {:?}", self.file_path);

        // Check if file exists and get its size
        let metadata = tokio::fs::metadata(&self.file_path).await?;
        println!("📊 File size: {} bytes", metadata.len());

        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        // Open file on first read
        if self.file.is_none() {
            self.file = Some(File::open(&self.file_path).await?);
        }

        let file = self.file.as_mut().unwrap();

        // Read into the provided buffer - this is the key BYOB benefit!
        // We're using the caller's buffer instead of allocating our own
        let bytes_read = file.read(buffer).await?;
        self.total_bytes_read += bytes_read as u64;

        if bytes_read == 0 {
            // End of file reached
            controller.close()?;
            println!(
                "✅ File reading complete: {} total bytes",
                self.total_bytes_read
            );
        } else {
            println!(
                "📖 Read {} bytes into provided buffer (total: {})",
                bytes_read, self.total_bytes_read
            );
        }

        Ok(bytes_read)
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("🚫 File reading cancelled: {:?}", reason);
        if let Some(mut file) = self.file.take() {
            // Tokio files don't need explicit closing, but we could do cleanup here
        }
        Ok(())
    }
}

/// A statistics tracker that counts bytes without allocating
pub struct ByteCountingSink {
    total_bytes: u64,
    chunk_count: u64,
}

impl ByteCountingSink {
    pub fn new() -> Self {
        Self {
            total_bytes: 0,
            chunk_count: 0,
        }
    }
}

impl WritableSink<Vec<u8>> for ByteCountingSink {
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.total_bytes += chunk.len() as u64;
        self.chunk_count += 1;

        println!(
            "📊 Processed chunk {} ({} bytes, {} total)",
            self.chunk_count,
            chunk.len(),
            self.total_bytes
        );

        // In a real app, you might process the bytes here
        // For demo, we just count them
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        println!("📋 Final statistics:");
        println!("   Total bytes processed: {}", self.total_bytes);
        println!("   Total chunks: {}", self.chunk_count);
        println!(
            "   Average chunk size: {:.1} bytes",
            self.total_bytes as f64 / self.chunk_count as f64
        );
        Ok(())
    }
}

/// Create a sample large file for testing BYOB performance
async fn create_sample_file(path: &str, size_mb: usize) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::io::AsyncWriteExt;

    let mut file = File::create(path).await?;
    let chunk = "A".repeat(1024); // 1KB chunks
    let chunks_needed = size_mb * 1024; // MB to KB conversion

    println!("📝 Creating {}MB sample file: {}", size_mb, path);

    for i in 0..chunks_needed {
        file.write_all(chunk.as_bytes()).await?;
        if i % 100 == 0 {
            print!(".");
            use tokio::io::{AsyncWriteExt, stdout};
            stdout().flush().await?;
        }
    }

    file.sync_all().await?;
    println!("\n✅ Sample file created successfully");
    Ok(())
}

/// Demonstrate BYOB reading with controlled buffer sizes
pub async fn run_byob_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== BYOB File Reader Example ===");

    let sample_file = "large_sample_file.txt";

    // Create a sample file for demonstration (5MB)
    create_sample_file(sample_file, 5).await?;

    // Create the BYOB readable stream
    //let byob_stream = ReadableByteStream::new(LargeFileByobSource::new(sample_file));
    let byob_stream = ReadableStream::builder(LargeFileByobSource::new(sample_file))
        .with_spawn(|fut| {
            tokio::spawn(fut);
        })
        .build();

    // Create a byte counter sink to process the data
    let counting_sink = WritableStream::new_with_spawn(
        ByteCountingSink::new(),
        Box::new(CountQueuingStrategy::new(5)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    println!("🚀 Starting BYOB reading with controlled buffer sizes...");

    // Get a BYOB reader and demonstrate controlled buffer usage
    //let reader = byob_stream.get_reader_byob()?;
    let reader = byob_stream.get_byob_reader().1;

    // Use different buffer sizes to show BYOB flexibility
    let buffer_sizes = vec![4096, 8192, 16384]; // 4KB, 8KB, 16KB
    let mut current_size_index = 0;

    // Manual reading loop to demonstrate BYOB control
    loop {
        // Cycle through different buffer sizes
        let buffer_size = buffer_sizes[current_size_index % buffer_sizes.len()];
        current_size_index += 1;

        // Allocate buffer of specific size - this is what we control with BYOB
        let mut buffer = vec![0u8; buffer_size];

        println!("🔄 Reading with {} byte buffer...", buffer_size);

        // Read into our specific buffer
        match reader.read(&mut buffer).await {
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    println!("📄 End of file reached");
                    break;
                }

                // Resize buffer to actual data read and write to sink
                buffer.truncate(bytes_read);

                // In a real scenario, you'd process the buffer here
                // For demo, we'll write to our counting sink
                let (sink, writer) = counting_sink.get_writer()?;
                writer.write(buffer).await?;
            }
            Err(e) => {
                println!("❌ Read error: {}", e);
                break;
            }
        }
    }

    // Close the sink to see final statistics
    if let Ok((_, writer)) = counting_sink.get_writer() {
        writer.close().await?;
    }

    // Clean up sample file
    if let Err(e) = tokio::fs::remove_file(sample_file).await {
        println!("⚠️ Could not remove sample file: {}", e);
    }

    println!("✅ BYOB example completed!");

    Ok(())
}

/// Compare BYOB vs regular reading performance
pub async fn run_byob_performance_comparison() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n=== BYOB Performance Comparison ===");

    let sample_file = "perf_test_file.txt";
    create_sample_file(sample_file, 10).await?; // 10MB file

    // Test 1: BYOB with buffer reuse
    println!("🏁 Test 1: BYOB with buffer reuse");
    let start = std::time::Instant::now();

    //let byob_stream = ReadableByteStream::new(LargeFileByobSource::new(sample_file));
    //let reader = byob_stream.get_reader_byob()?;
    let byob_stream = ReadableStream::builder(LargeFileByobSource::new(sample_file))
        .with_spawn(|fut| {
            tokio::spawn(fut);
        })
        .build();
    let reader = byob_stream.get_byob_reader().1;

    // Reuse the same buffer - key BYOB advantage
    let mut reusable_buffer = vec![0u8; 64 * 1024]; // 64KB buffer
    let mut total_bytes = 0u64;

    loop {
        match reader.read(&mut reusable_buffer).await {
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    break;
                }
                total_bytes += bytes_read as u64;
                // Buffer is reused - no allocation overhead!
            }
            Err(_) => break,
        }
    }

    let byob_duration = start.elapsed();
    println!("   BYOB: {} bytes in {:?}", total_bytes, byob_duration);

    // Test 2: Regular reading (would allocate new Vec<u8> each time)
    println!("🏁 Test 2: Regular stream reading (simulated)");
    let start = std::time::Instant::now();

    // This simulates what happens with regular default readers
    let mut file = File::open(sample_file).await?;
    let mut total_bytes_regular = 0u64;

    loop {
        // Each read allocates a new buffer - this is what BYOB avoids
        let mut buffer = vec![0u8; 64 * 1024];
        match file.read(&mut buffer).await {
            Ok(bytes_read) => {
                if bytes_read == 0 {
                    break;
                }
                total_bytes_regular += bytes_read as u64;
                // buffer gets dropped here - allocation overhead each iteration
            }
            Err(_) => break,
        }
    }

    let regular_duration = start.elapsed();
    println!(
        "   Regular: {} bytes in {:?}",
        total_bytes_regular, regular_duration
    );

    // Show performance difference
    if byob_duration < regular_duration {
        let improvement = regular_duration.as_secs_f64() / byob_duration.as_secs_f64();
        println!("📈 BYOB was {:.2}x faster!", improvement);
    } else {
        println!("📊 Performance similar (overhead dominated by I/O)");
    }

    // Clean up
    let _ = tokio::fs::remove_file(sample_file).await;

    println!("✅ Performance comparison completed!");

    Ok(())
}

/// Demonstrate when NOT to use BYOB
pub async fn demonstrate_byob_antipatterns() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n=== When NOT to Use BYOB ===");

    println!("❌ Anti-pattern 1: Small files with infrequent reads");
    println!("   - BYOB overhead > allocation savings");
    println!("   - Use regular ReadableStream<Vec<u8>> instead");

    println!("❌ Anti-pattern 2: Data transformations");
    println!("   - Transforms need to process data, not manage buffers");
    println!("   - Use TransformStream with default readers");

    println!("❌ Anti-pattern 3: Piping operations");
    println!("   - pipeTo/pipeThrough use default readers internally");
    println!("   - BYOB benefits are lost in piping");

    println!("✅ Use BYOB for:");
    println!("   - Large file processing with manual reading");
    println!("   - Network streams with buffer reuse");
    println!("   - Performance-critical scenarios with measurable allocation overhead");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_byob_example().await?;
    run_byob_performance_comparison().await?;
    demonstrate_byob_antipatterns().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    #[tokio::test]
    async fn test_byob_file_reading() {
        let test_file = "test_byob_file.txt";
        let test_data = "Hello, BYOB world! ".repeat(1000);

        // Create test file
        fs::write(test_file, &test_data).await.unwrap();

        // Test BYOB reading
        //let byob_stream = ReadableByteStream::new(LargeFileByobSource::new(test_file));
        //let reader = byob_stream.get_reader_byob().unwrap();
        let byob_stream = ReadableStream::builder(LargeFileByobSource::new(test_file))
            .with_spawn(|fut| {
                tokio::spawn(fut);
            })
            .build();
        let reader = byob_stream.get_byob_reader().1;

        let mut total_read = Vec::new();
        let mut buffer = vec![0u8; 256]; // Small buffer for testing

        loop {
            match reader.read(&mut buffer).await {
                Ok(bytes_read) => {
                    if bytes_read == 0 {
                        break;
                    }
                    total_read.extend_from_slice(&buffer[..bytes_read]);
                }
                Err(_) => break,
            }
        }

        // Verify data integrity
        let read_string = String::from_utf8(total_read).unwrap();
        assert_eq!(read_string, test_data);

        // Clean up
        let _ = fs::remove_file(test_file).await;
    }

    #[tokio::test]
    async fn test_byob_empty_file() {
        let test_file = "test_empty_byob.txt";

        // Create empty test file
        fs::write(test_file, "").await.unwrap();

        //let byob_stream = ReadableByteStream::new(LargeFileByobSource::new(test_file));
        //let reader = byob_stream.get_reader_byob().unwrap();
        let byob_stream = ReadableStream::builder(LargeFileByobSource::new(test_file))
            .with_spawn(|fut| {
                tokio::spawn(fut);
            })
            .build();
        let reader = byob_stream.get_byob_reader().1;

        let mut buffer = vec![0u8; 1024];
        let bytes_read = reader.read(&mut buffer).await.unwrap();

        assert_eq!(bytes_read, 0); // Should immediately return 0 for empty file

        // Clean up
        let _ = fs::remove_file(test_file).await;
    }

    #[tokio::test]
    async fn test_byob_different_buffer_sizes() {
        let test_file = "test_buffer_sizes.txt";
        let test_data = "X".repeat(10000); // 10KB of data

        fs::write(test_file, &test_data).await.unwrap();

        let buffer_sizes = vec![512, 1024, 4096, 8192];

        for buffer_size in buffer_sizes {
            //let byob_stream = ReadableByteStream::new(LargeFileByobSource::new(test_file));
            let byob_stream = ReadableStream::builder(LargeFileByobSource::new(test_file))
                .with_spawn(|fut| {
                    tokio::spawn(fut);
                })
                .build();
            //let reader = byob_stream.get_reader_byob().unwrap();
            let reader = byob_stream.get_byob_reader().1;

            let mut total_read = Vec::new();
            let mut buffer = vec![0u8; buffer_size];

            loop {
                match reader.read(&mut buffer).await {
                    Ok(bytes_read) => {
                        if bytes_read == 0 {
                            break;
                        }
                        total_read.extend_from_slice(&buffer[..bytes_read]);
                    }
                    Err(_) => break,
                }
            }

            assert_eq!(
                total_read.len(),
                test_data.len(),
                "Failed with buffer size {}",
                buffer_size
            );
        }

        // Clean up
        let _ = fs::remove_file(test_file).await;
    }
}
