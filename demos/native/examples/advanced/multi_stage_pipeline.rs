//! Multi-Stage Pipeline Example
//!
//! Demonstrates a complex data processing pipeline that chains multiple transforms:
//! 1. Raw log data source
//! 2. Parse log entries into structured data
//! 3. Filter by log level
//! 4. Add timestamps and metadata  
//! 5. Convert to JSON format
//! 6. Compress the output
//! 7. Write to file
//!
//! This example shows:
//! - How to chain multiple transform streams
//! - Different data types flowing through the pipeline (String -> LogEntry -> String)
//! - Error handling and backpressure across multiple stages
//! - Real-world pipeline complexity

use chrono::{DateTime, Utc};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::io::Write;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    transform::{TransformStream, TransformStreamDefaultController, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Structured log entry after parsing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: Option<DateTime<Utc>>,
    pub level: LogLevel,
    pub message: String,
    pub source: Option<String>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LogLevel {
    ERROR,
    WARN,
    INFO,
    DEBUG,
    TRACE,
}

impl LogLevel {
    fn from_str(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ERROR" => Some(LogLevel::ERROR),
            "WARN" | "WARNING" => Some(LogLevel::WARN),
            "INFO" => Some(LogLevel::INFO),
            "DEBUG" => Some(LogLevel::DEBUG),
            "TRACE" => Some(LogLevel::TRACE),
            _ => None,
        }
    }
}

/// Stage 1: Raw log data source
pub struct LogDataSource {
    logs: Vec<String>,
    index: usize,
}

impl LogDataSource {
    pub fn new() -> Self {
        let sample_logs = vec![
            "2024-01-15T10:30:45Z [ERROR] [auth-service] [thread-1] User authentication failed: invalid credentials",
            "2024-01-15T10:30:46Z [INFO] [web-server] [thread-2] Request processed successfully: GET /api/users",
            "2024-01-15T10:30:47Z [DEBUG] [database] [thread-3] Connection pool stats: active=5, idle=15, max=20",
            "2024-01-15T10:30:48Z [ERROR] [payment-service] [thread-4] Payment processing failed: timeout after 30s",
            "2024-01-15T10:30:49Z [WARN] [cache] [thread-1] Cache miss rate high: 85% over last 5 minutes",
            "2024-01-15T10:30:50Z [INFO] [scheduler] [thread-5] Background job completed: data-cleanup",
            "2024-01-15T10:30:51Z [TRACE] [network] [thread-2] TCP packet sent: 1024 bytes to 192.168.1.100",
            "2024-01-15T10:30:52Z [ERROR] [file-system] [thread-6] File not found: /var/logs/app.log",
            "2024-01-15T10:30:53Z [DEBUG] [memory] [thread-3] Memory usage: 2.1GB used, 1.9GB free",
            "2024-01-15T10:30:54Z [INFO] [health-check] [thread-7] All systems operational",
        ];

        Self {
            logs: sample_logs.into_iter().map(String::from).collect(),
            index: 0,
        }
    }
}

impl ReadableSource<String> for LogDataSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.index < self.logs.len() {
            let log_line = self.logs[self.index].clone();
            self.index += 1;
            controller.enqueue(log_line)?;

            // Simulate real-time log generation with small delay
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Stage 2: Parse raw log strings into structured LogEntry objects
pub struct LogParser;

impl Transformer<String, LogEntry> for LogParser {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<LogEntry>,
    ) -> StreamResult<()> {
        // Parse log line: "timestamp [level] [source] [thread] message"
        let parts: Vec<&str> = chunk.splitn(5, ' ').collect();

        if parts.len() >= 5 {
            let timestamp = DateTime::parse_from_rfc3339(parts[0])
                .ok()
                .map(|dt| dt.with_timezone(&Utc));

            // Extract level from [LEVEL]
            let level_str = parts[1].trim_start_matches('[').trim_end_matches(']');
            let level = LogLevel::from_str(level_str).unwrap_or(LogLevel::INFO);

            // Extract source from [source]
            let source = Some(
                parts[2]
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string(),
            );

            // Extract thread from [thread]
            let thread_id = Some(
                parts[3]
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .to_string(),
            );

            let message = parts[4].to_string();

            let log_entry = LogEntry {
                timestamp,
                level,
                message,
                source,
                thread_id,
            };

            controller.enqueue(log_entry.clone())?;
            println!("📝 Parsed: {} -> {:?}", level_str, log_entry.level);
        } else {
            println!("⚠️  Failed to parse log line: {}", chunk);
            // Skip malformed lines rather than error
        }

        Ok(())
    }
}

/// Stage 3: Filter logs by level (ERROR and WARN only)
pub struct LogLevelFilter {
    allowed_levels: Vec<LogLevel>,
}

impl LogLevelFilter {
    pub fn new(levels: Vec<LogLevel>) -> Self {
        Self {
            allowed_levels: levels,
        }
    }
}

impl Transformer<LogEntry, LogEntry> for LogLevelFilter {
    async fn transform(
        &mut self,
        chunk: LogEntry,
        controller: &mut TransformStreamDefaultController<LogEntry>,
    ) -> StreamResult<()> {
        if self.allowed_levels.contains(&chunk.level) {
            println!("✅ Passed filter: {:?} - {}", chunk.level, chunk.message);
            controller.enqueue(chunk)?;
        } else {
            println!("🚫 Filtered out: {:?}", chunk.level);
            // Don't enqueue - filter out this entry
        }
        Ok(())
    }
}

/// Stage 4: Add processing metadata and enrich entries
pub struct MetadataEnricher {
    processing_id: String,
    processed_count: u64,
}

impl MetadataEnricher {
    pub fn new() -> Self {
        Self {
            processing_id: format!("proc-{}", uuid::Uuid::new_v4().simple()),
            processed_count: 0,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct EnrichedLogEntry {
    #[serde(flatten)]
    log_entry: LogEntry,
    processing_metadata: ProcessingMetadata,
}

#[derive(Serialize, Deserialize)]
struct ProcessingMetadata {
    processing_id: String,
    processing_timestamp: DateTime<Utc>,
    sequence_number: u64,
    pipeline_stage: String,
}

impl Transformer<LogEntry, String> for MetadataEnricher {
    async fn transform(
        &mut self,
        chunk: LogEntry,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        self.processed_count += 1;

        let enriched = EnrichedLogEntry {
            log_entry: chunk,
            processing_metadata: ProcessingMetadata {
                processing_id: self.processing_id.clone(),
                processing_timestamp: Utc::now(),
                sequence_number: self.processed_count,
                pipeline_stage: "multi-stage-pipeline".to_string(),
            },
        };

        // Convert to JSON string
        match serde_json::to_string(&enriched) {
            Ok(json_str) => {
                println!("🔄 Enriched entry #{}", self.processed_count);
                controller.enqueue(json_str)?;
            }
            Err(e) => {
                println!("❌ JSON serialization failed: {}", e);
                // Continue processing rather than failing the entire pipeline
            }
        }

        Ok(())
    }
}

/// Stage 5: Compression transform (reusing from previous example)
pub struct JsonGzipCompressor {
    encoder: Option<GzEncoder<Vec<u8>>>,
    entries_compressed: u64,
}

impl JsonGzipCompressor {
    pub fn new() -> Self {
        Self {
            encoder: None,
            entries_compressed: 0,
        }
    }
}

impl Transformer<String, Vec<u8>> for JsonGzipCompressor {
    async fn start(
        &mut self,
        _controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.encoder = Some(GzEncoder::new(Vec::new(), Compression::default()));
        println!("🗜️  JSON Gzip compressor initialized");
        Ok(())
    }

    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if let Some(encoder) = &mut self.encoder {
            self.entries_compressed += 1;

            // Add newline to separate JSON entries
            let json_line = format!("{}\n", chunk);

            encoder
                .write_all(json_line.as_bytes())
                .map_err(|e| format!("Compression error: {}", e))?;

            // Periodically flush to get compressed output
            if self.entries_compressed % 5 == 0 {
                encoder
                    .flush()
                    .map_err(|e| format!("Compression flush error: {}", e))?;

                let compressed_data = encoder.get_ref().clone();
                if !compressed_data.is_empty() {
                    controller.enqueue(compressed_data.clone())?;
                    // Reset encoder with new buffer
                    *encoder = GzEncoder::new(Vec::new(), Compression::default());
                    println!("🗜️  Compressed batch of 5 entries");
                }
            }
        }
        Ok(())
    }

    async fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if let Some(encoder) = self.encoder.take() {
            let final_compressed = encoder
                .finish()
                .map_err(|e| format!("Final compression error: {}", e))?;

            if !final_compressed.is_empty() {
                controller.enqueue(final_compressed)?;
            }

            println!(
                "✅ Compression complete - {} entries processed",
                self.entries_compressed
            );
        }
        Ok(())
    }
}

/// Final stage: Compressed file writer
pub struct CompressedJsonWriter {
    file_path: String,
    file_handle: Option<File>,
    bytes_written: u64,
}

impl CompressedJsonWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file_handle: None,
            bytes_written: 0,
        }
    }
}

impl WritableSink<Vec<u8>> for CompressedJsonWriter {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.file_handle = Some(File::create(&self.file_path).await?);
        println!("📁 Created compressed JSON output: {}", self.file_path);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file_handle {
            file.write_all(&chunk).await?;
            self.bytes_written += chunk.len() as u64;
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(file) = self.file_handle.take() {
            file.sync_all().await?;
            println!(
                "🔒 Pipeline output complete: {} bytes written to {}",
                self.bytes_written, self.file_path
            );
        }
        Ok(())
    }
}

/// Run the complete multi-stage pipeline
pub async fn run_multi_stage_pipeline() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Multi-Stage Processing Pipeline ===");
    println!("📊 Pipeline stages:");
    println!("   1. Raw log data source");
    println!("   2. Parse to structured entries");
    println!("   3. Filter by log level (ERROR, WARN only)");
    println!("   4. Add processing metadata");
    println!("   5. Compress as JSON");
    println!("   6. Write to file");
    println!();

    // Stage 1: Create data source
    //let log_source = ReadableStream::new_default(LogDataSource::new(), None);
    let log_source = ReadableStream::builder(LogDataSource::new())
        .with_spawn(|fut| {
            tokio::spawn(fut);
        })
        .build();

    // Stage 2: Parse logs into structured data
    let parser = TransformStream::new(LogParser);

    // Stage 3: Filter by level
    let filter = TransformStream::new(LogLevelFilter::new(vec![LogLevel::ERROR, LogLevel::WARN]));

    // Stage 4: Add metadata and convert to JSON
    let enricher = TransformStream::new(MetadataEnricher::new());

    // Stage 5: Compress JSON
    let compressor = TransformStream::new(JsonGzipCompressor::new());

    // Stage 6: Write compressed output
    let output_file = "processed_logs.jsonl.gz";
    let writer = WritableStream::new_with_spawn(
        CompressedJsonWriter::new(output_file),
        Box::new(CountQueuingStrategy::new(3)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    println!("🚀 Starting pipeline execution...");
    let start_time = std::time::Instant::now();

    // Execute the complete pipeline
    log_source
        .pipe_through(parser, None) // String -> LogEntry
        .pipe_through(filter, None) // LogEntry -> LogEntry (filtered)
        .pipe_through(enricher, None) // LogEntry -> String (JSON)
        .pipe_through(compressor, None) // String -> Vec<u8> (compressed)
        .pipe_to(&writer, None) // Vec<u8> -> File
        .await?;

    let duration = start_time.elapsed();
    println!("✅ Pipeline completed in {:?}", duration);

    // Verify the output
    verify_pipeline_output(output_file).await?;

    Ok(())
}

/// Verify the pipeline output by decompressing and checking the data
async fn verify_pipeline_output(compressed_file: &str) -> Result<(), Box<dyn std::error::Error>> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    println!("🔍 Verifying pipeline output...");

    let compressed_data = tokio::fs::read(compressed_file).await?;
    let mut decoder = GzDecoder::new(&compressed_data[..]);
    let mut decompressed = String::new();
    decoder.read_to_string(&mut decompressed)?;

    let lines: Vec<&str> = decompressed
        .trim()
        .split('\n')
        .filter(|s| !s.is_empty())
        .collect();
    println!("📄 Output contains {} processed log entries", lines.len());

    // Parse and validate a few entries
    for (i, line) in lines.iter().take(3).enumerate() {
        match serde_json::from_str::<EnrichedLogEntry>(line) {
            Ok(parsed) => {
                println!(
                    "   Entry {}: {:?} - {}",
                    i + 1,
                    parsed.log_entry.level,
                    parsed.processing_metadata.sequence_number
                );
            }
            Err(e) => {
                println!("   Entry {}: JSON parse error - {}", i + 1, e);
            }
        }
    }

    let file_size = tokio::fs::metadata(compressed_file).await?.len();
    println!("💾 Compressed file size: {} bytes", file_size);
    println!(
        "📈 Compression ratio: {:.1}%",
        (file_size as f64 / decompressed.len() as f64) * 100.0
    );

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_multi_stage_pipeline().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    #[tokio::test]
    async fn test_log_parser() {
        let parser_stream = TransformStream::new(LogParser);
        let (readable, writable) = parser_stream.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        let test_log =
            "2024-01-15T10:30:45Z [ERROR] [auth-service] [thread-1] Authentication failed";
        writer.write(test_log.to_string()).await.unwrap();
        writer.close().await.unwrap();

        let result = reader.read().await.unwrap().unwrap();
        assert_eq!(result.level, LogLevel::ERROR);
        assert!(result.message.contains("Authentication failed"));
    }

    #[tokio::test]
    async fn test_level_filter() {
        let filter = TransformStream::new(LogLevelFilter::new(vec![LogLevel::ERROR]));
        let (readable, writable) = filter.split();
        let (_, writer) = writable.get_writer().unwrap();
        let (_, reader) = readable.get_reader();

        // Test entries
        let error_entry = LogEntry {
            timestamp: None,
            level: LogLevel::ERROR,
            message: "Error message".to_string(),
            source: None,
            thread_id: None,
        };

        let info_entry = LogEntry {
            timestamp: None,
            level: LogLevel::INFO,
            message: "Info message".to_string(),
            source: None,
            thread_id: None,
        };

        writer.write(error_entry.clone()).await.unwrap();
        writer.write(info_entry).await.unwrap();
        writer.close().await.unwrap();

        // Should only get the ERROR entry
        let result = reader.read().await.unwrap().unwrap();
        assert_eq!(result.level, LogLevel::ERROR);

        // No more entries should be available
        let result2 = reader.read().await.unwrap();
        assert!(result2.is_none());
    }

    #[tokio::test]
    async fn test_complete_mini_pipeline() {
        // Test a simplified 3-stage pipeline
        let source = ReadableStream::new_default(LogDataSource::new(), None);
        let parser = TransformStream::new(LogParser);
        let filter =
            TransformStream::new(LogLevelFilter::new(vec![LogLevel::ERROR, LogLevel::WARN]));

        let output_file = "test_pipeline_output.jsonl";
        let writer = WritableStream::new_with_spawn(
            TestJsonWriter::new(output_file),
            Box::new(CountQueuingStrategy::new(5)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        // Run mini pipeline: logs -> parse -> filter -> write
        source
            .pipe_through(parser, None)
            .pipe_through(filter, None)
            .pipe_to(&TestJsonWriteAdapter::new(writer), None)
            .await
            .expect("Mini pipeline should complete");

        // Verify output exists
        let content = fs::read_to_string(output_file).await.unwrap();
        assert!(!content.is_empty());

        // Should contain only ERROR and WARN entries
        assert!(content.contains("ERROR") || content.contains("WARN"));
        assert!(!content.contains("INFO")); // INFO should be filtered out

        // Clean up
        let _ = fs::remove_file(output_file).await;
    }

    // Helper test writer for structured data
    struct TestJsonWriter {
        file_path: String,
        content: String,
    }

    impl TestJsonWriter {
        fn new(path: &str) -> Self {
            Self {
                file_path: path.to_string(),
                content: String::new(),
            }
        }
    }

    impl WritableSink<LogEntry> for TestJsonWriter {
        async fn write(
            &mut self,
            chunk: LogEntry,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            let json_line = serde_json::to_string(&chunk).unwrap();
            self.content.push_str(&json_line);
            self.content.push('\n');
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            tokio::fs::write(&self.file_path, self.content).await?;
            Ok(())
        }
    }

    // Adapter to bridge LogEntry -> String for existing writer
    struct TestJsonWriteAdapter {
        inner: WritableStream<String, TestStringWriter, whatwg_streams::dlc::ideation::d::Unlocked>,
    }

    impl TestJsonWriteAdapter {
        fn new(
            _writer: WritableStream<
                Vec<u8>,
                CompressedJsonWriter,
                whatwg_streams::dlc::ideation::d::Unlocked,
            >,
        ) -> Self {
            // Create a simple string writer for testing
            let string_writer = WritableStream::new_with_spawn(
                TestStringWriter::new("test_adapter_output.txt"),
                Box::new(CountQueuingStrategy::new(5)),
                |fut| {
                    tokio::spawn(fut);
                },
            );

            Self {
                inner: string_writer,
            }
        }
    }

    struct TestStringWriter {
        file_path: String,
        content: String,
    }

    impl TestStringWriter {
        fn new(path: &str) -> Self {
            Self {
                file_path: path.to_string(),
                content: String::new(),
            }
        }
    }

    impl WritableSink<String> for TestStringWriter {
        async fn write(
            &mut self,
            chunk: String,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.content.push_str(&chunk);
            self.content.push('\n');
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            tokio::fs::write(&self.file_path, self.content).await?;
            Ok(())
        }
    }
}
