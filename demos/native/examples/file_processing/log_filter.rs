//! Log Filter Example
//!
//! Demonstrates a complete pipeline that:
//! 1. Reads log lines from a mock data source
//! 2. Filters to only ERROR lines using a transform stream
//! 3. Writes filtered results to a file
//!
//! This example shows:
//! - Basic readable stream from in-memory data
//! - Transform stream for filtering data
//! - Writable stream for file output
//! - Complete pipeline using pipe_through and pipe_to

use futures::future::ready;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult, Unlocked,
    readable_sampling_b::{
        DefaultStream, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    },
    transform::{TransformStream, TransformStreamDefaultController, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A readable source that simulates reading log lines from a file
/// In a real application, this would read from an actual file asynchronously
pub struct LogFileSource {
    lines: Vec<String>,
    index: usize,
}

impl LogFileSource {
    pub fn new(log_data: Vec<&str>) -> Self {
        Self {
            lines: log_data.into_iter().map(String::from).collect(),
            index: 0,
        }
    }
}

impl ReadableSource<String> for LogFileSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.index < self.lines.len() {
            let line = self.lines[self.index].clone();
            self.index += 1;
            controller.enqueue(line)?;
        } else {
            // No more data - close the stream
            controller.close()?;
        }
        Ok(())
    }
}

/// A transform stream that filters log lines to only pass through ERROR messages
pub struct ErrorFilterTransformer;

impl Transformer<String, String> for ErrorFilterTransformer {
    fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> impl futures::Future<Output = StreamResult<()>> + Send + 'static {
        futures::future::ready(if chunk.contains("ERROR") {
            // Pass through ERROR lines
            controller.enqueue(chunk)
        } else {
            // Filter out non-ERROR lines (no output)
            Ok(())
        })
    }
}

/// A writable sink that writes each chunk to a file, creating the file on first write
pub struct FileWriterSink {
    file_path: String,
    file_handle: Option<File>,
}

impl FileWriterSink {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file_handle: None,
        }
    }
}

impl WritableSink<String> for FileWriterSink {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        // Create the file when the stream starts
        self.file_handle = Some(File::create(&self.file_path).await?);
        println!("Created output file: {}", self.file_path);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file_handle {
            file.write_all(chunk.as_bytes()).await?;
            file.write_all(b"\n").await?; // Add newline after each log entry
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(file) = self.file_handle.take() {
            file.sync_all().await?; // Ensure all data is written to disk
            println!("File closed and synced successfully");
        }
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("File writer aborted: {:?}", reason);
        if let Some(file) = self.file_handle.take() {
            file.sync_all().await?;
        }
        Ok(())
    }
}

/// Example function demonstrating the complete log filtering pipeline
pub async fn run_log_filter_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Log Filter Example ===");

    // Sample log data - mix of different log levels
    let log_data = vec![
        "2024-01-01 10:00:00 INFO: Application started successfully",
        "2024-01-01 10:05:23 ERROR: User authentication failed for user 'alice'",
        "2024-01-01 10:07:45 DEBUG: Database connection pool initialized with 10 connections",
        "2024-01-01 10:12:33 ERROR: Database connection timeout after 30 seconds",
        "2024-01-01 10:15:22 WARN: Memory usage is at 85% - consider scaling",
        "2024-01-01 10:18:44 ERROR: File not found: /tmp/missing_file.txt",
        "2024-01-01 10:20:11 INFO: Background job completed successfully",
    ];

    println!("Processing {} log lines...", log_data.len());

    // Create the readable stream from our mock log data
    let readable_stream = ReadableStream::new_default(LogFileSource::new(log_data), None);

    // Create the transform stream that filters for ERROR lines only
    let error_filter = TransformStream::new(ErrorFilterTransformer);

    // Create the writable stream that outputs to a file
    let output_file_path = "filtered_errors.log";
    let writable_stream = WritableStream::new_with_spawn(
        FileWriterSink::new(output_file_path),
        Box::new(CountQueuingStrategy::new(5)), // Buffer up to 5 chunks
        |fut| {
            tokio::spawn(fut); // Spawn the sink operations on tokio runtime
        },
    );

    // Execute the complete pipeline:
    // Readable -> Transform (filter) -> Writable
    readable_stream
        .pipe_through(error_filter, None)
        .pipe_to(&writable_stream, None)
        .await?;

    println!("✅ Pipeline completed successfully!");
    println!("📁 Filtered errors written to: {}", output_file_path);

    // Verify the results by reading back the file
    match tokio::fs::read_to_string(output_file_path).await {
        Ok(content) => {
            println!("📄 File contents:");
            println!("{}", content);

            // Count the lines to verify filtering worked
            let line_count = content.lines().count();
            println!("📊 Filtered {} ERROR lines from original log", line_count);
        }
        Err(e) => println!("⚠️  Could not read output file: {}", e),
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_log_filter_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    #[tokio::test]
    async fn test_log_filter_pipeline() {
        let test_logs = vec![
            "INFO: Normal operation",
            "ERROR: Something went wrong",
            "DEBUG: Detailed info",
            "ERROR: Another error",
            "WARN: Warning message",
        ];

        let readable = ReadableStream::new_default(LogFileSource::new(test_logs), None);
        let transform = TransformStream::new(ErrorFilterTransformer);
        let output_path = "test_filtered.log";
        let writable = WritableStream::new_with_spawn(
            FileWriterSink::new(output_path),
            Box::new(CountQueuingStrategy::new(5)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        // Run the pipeline
        readable
            .pipe_through(transform, None)
            .pipe_to(&writable, None)
            .await
            .expect("Pipeline should complete successfully");

        // Verify only ERROR lines were written
        let content = fs::read_to_string(output_path).await.unwrap();
        let lines: Vec<&str> = content.trim().split('\n').collect();

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("ERROR: Something went wrong"));
        assert!(lines[1].contains("ERROR: Another error"));

        // Clean up
        let _ = fs::remove_file(output_path).await;
    }

    #[tokio::test]
    async fn test_empty_log_source() {
        let readable = ReadableStream::new_default(LogFileSource::new(vec![]), None);
        let transform = TransformStream::new(ErrorFilterTransformer);
        let output_path = "test_empty.log";
        let writable = WritableStream::new_with_spawn(
            FileWriterSink::new(output_path),
            Box::new(CountQueuingStrategy::new(1)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        readable
            .pipe_through(transform, None)
            .pipe_to(&writable, None)
            .await
            .expect("Empty pipeline should complete");

        // File should exist but be empty
        let content = fs::read_to_string(output_path).await.unwrap();
        assert!(content.is_empty());

        // Clean up
        let _ = fs::remove_file(output_path).await;
    }
}
