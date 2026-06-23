//! A multi-stage pipeline that changes data type at each step.
//!
//! Raw log lines flow through five transforms before hitting a file sink:
//!   source(String) -> parse(LogEntry) -> filter(LogEntry) -> enrich(String/JSON)
//!   -> gzip(Vec<u8>) -> file
//!
//! Each `pipe_through` hands the next stage the previous stage's readable side, so the
//! whole chain runs concurrently with backpressure applied end to end. The output is
//! gzip-compressed NDJSON (one JSON object per line).
//!
//! Run with: `cargo run --example multi_stage_pipeline`

use chrono::{DateTime, Utc};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::io::Write;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: Option<DateTime<Utc>>,
    pub level: LogLevel,
    pub message: String,
    pub source: Option<String>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn parse(s: &str) -> Option<Self> {
        match s.to_uppercase().as_str() {
            "ERROR" => Some(LogLevel::Error),
            "WARN" | "WARNING" => Some(LogLevel::Warn),
            "INFO" => Some(LogLevel::Info),
            "DEBUG" => Some(LogLevel::Debug),
            "TRACE" => Some(LogLevel::Trace),
            _ => None,
        }
    }
}

/// Stage 1 — yields raw log lines, then closes.
#[derive(Default)]
pub struct LogDataSource {
    logs: Vec<String>,
    index: usize,
}

impl LogDataSource {
    pub fn new() -> Self {
        let sample = [
            "2024-01-15T10:30:45Z [ERROR] [auth-service] [thread-1] Authentication failed: invalid credentials",
            "2024-01-15T10:30:46Z [INFO] [web-server] [thread-2] Request processed: GET /api/users",
            "2024-01-15T10:30:47Z [DEBUG] [database] [thread-3] Pool stats: active=5 idle=15 max=20",
            "2024-01-15T10:30:48Z [ERROR] [payment-service] [thread-4] Payment failed: timeout after 30s",
            "2024-01-15T10:30:49Z [WARN] [cache] [thread-1] Cache miss rate high: 85% over 5 minutes",
            "2024-01-15T10:30:50Z [INFO] [scheduler] [thread-5] Background job completed: data-cleanup",
            "2024-01-15T10:30:51Z [TRACE] [network] [thread-2] TCP packet sent: 1024 bytes",
            "2024-01-15T10:30:52Z [ERROR] [file-system] [thread-6] File not found: /var/logs/app.log",
        ];
        Self {
            logs: sample.into_iter().map(String::from).collect(),
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
            controller.enqueue(self.logs[self.index].clone())?;
            self.index += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Stage 2 — parses `timestamp [level] [source] [thread] message` into a `LogEntry`.
pub struct LogParser;

impl Transformer<String, LogEntry> for LogParser {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<LogEntry>,
    ) -> StreamResult<()> {
        let parts: Vec<&str> = chunk.splitn(5, ' ').collect();
        if parts.len() < 5 {
            return Ok(()); // skip malformed lines
        }
        let unbracket = |s: &str| s.trim_start_matches('[').trim_end_matches(']').to_string();
        let entry = LogEntry {
            timestamp: DateTime::parse_from_rfc3339(parts[0])
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            level: LogLevel::parse(&unbracket(parts[1])).unwrap_or(LogLevel::Info),
            source: Some(unbracket(parts[2])),
            thread_id: Some(unbracket(parts[3])),
            message: parts[4].to_string(),
        };
        controller.enqueue(entry)
    }
}

/// Stage 3 — forwards only entries whose level is in `allowed`.
pub struct LogLevelFilter {
    allowed: Vec<LogLevel>,
}

impl LogLevelFilter {
    pub fn new(allowed: Vec<LogLevel>) -> Self {
        Self { allowed }
    }
}

impl Transformer<LogEntry, LogEntry> for LogLevelFilter {
    async fn transform(
        &mut self,
        chunk: LogEntry,
        controller: &mut TransformStreamDefaultController<LogEntry>,
    ) -> StreamResult<()> {
        if self.allowed.contains(&chunk.level) {
            controller.enqueue(chunk)?;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct EnrichedLogEntry {
    #[serde(flatten)]
    entry: LogEntry,
    processing_id: String,
    sequence_number: u64,
}

/// Stage 4 — tags each entry with processing metadata and serializes it to JSON.
pub struct MetadataEnricher {
    processing_id: String,
    count: u64,
}

impl Default for MetadataEnricher {
    fn default() -> Self {
        Self {
            processing_id: format!("proc-{}", uuid::Uuid::new_v4().simple()),
            count: 0,
        }
    }
}

impl Transformer<LogEntry, String> for MetadataEnricher {
    async fn transform(
        &mut self,
        chunk: LogEntry,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        self.count += 1;
        let enriched = EnrichedLogEntry {
            entry: chunk,
            processing_id: self.processing_id.clone(),
            sequence_number: self.count,
        };
        let json = serde_json::to_string(&enriched).map_err(|e| e.to_string())?;
        controller.enqueue(json)
    }
}

/// Stage 5 — gzips each JSON line, emitting compressed bytes as they become available.
#[derive(Default)]
pub struct JsonlGzipCompressor {
    encoder: Option<GzEncoder<Vec<u8>>>,
    emitted: usize,
}

impl Transformer<String, Vec<u8>> for JsonlGzipCompressor {
    async fn start(
        &mut self,
        _c: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.encoder = Some(GzEncoder::new(Vec::new(), Compression::default()));
        Ok(())
    }

    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let encoder = self.encoder.as_mut().expect("encoder set in start");
        writeln!(encoder, "{chunk}")?;
        encoder.flush()?;
        let produced = encoder.get_ref();
        if produced.len() > self.emitted {
            let new_bytes = produced[self.emitted..].to_vec();
            self.emitted = produced.len();
            controller.enqueue(new_bytes)?;
        }
        Ok(())
    }

    async fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if let Some(encoder) = self.encoder.take() {
            let finished = encoder.finish()?;
            if finished.len() > self.emitted {
                controller.enqueue(finished[self.emitted..].to_vec())?;
            }
        }
        Ok(())
    }
}

/// Final stage — writes the compressed bytes to a file.
pub struct CompressedJsonWriter {
    path: String,
    file: Option<File>,
}

impl CompressedJsonWriter {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            file: None,
        }
    }
}

impl WritableSink<Vec<u8>> for CompressedJsonWriter {
    async fn start(&mut self, _c: &mut WritableStreamDefaultController) -> StreamResult<()> {
        self.file = Some(File::create(&self.path).await?);
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
    let output_file = "processed_logs.jsonl.gz";

    let source = ReadableStream::builder(LogDataSource::new()).spawn(tokio::spawn);
    let parser = TransformStream::builder(LogParser).spawn(tokio::spawn);
    let filter = TransformStream::builder(LogLevelFilter::new(vec![LogLevel::Error, LogLevel::Warn]))
        .spawn(tokio::spawn);
    let enricher = TransformStream::builder(MetadataEnricher::default()).spawn(tokio::spawn);
    let compressor = TransformStream::builder(JsonlGzipCompressor::default()).spawn(tokio::spawn);
    let writer = WritableStream::builder(CompressedJsonWriter::new(output_file)).spawn(tokio::spawn);

    println!("running: source -> parse -> filter(ERROR,WARN) -> enrich -> gzip -> file");
    let parsed = source.pipe_through(parser, None).spawn(tokio::spawn);
    let filtered = parsed.pipe_through(filter, None).spawn(tokio::spawn);
    let enriched = filtered.pipe_through(enricher, None).spawn(tokio::spawn);
    let compressed = enriched.pipe_through(compressor, None).spawn(tokio::spawn);
    compressed
        .pipe_to(&writer, None)
        .await
        .expect("pipeline should complete");

    verify_output(output_file).await;
}

/// Decompresses the output and reports how many entries survived the filter.
async fn verify_output(path: &str) {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let compressed = tokio::fs::read(path).await.expect("read output");
    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut text = String::new();
    decoder.read_to_string(&mut text).expect("decompress output");

    let entries: Vec<EnrichedLogEntry> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("parse NDJSON line"))
        .collect();

    println!(
        "wrote {path} ({} bytes): {} entries, levels {:?}",
        compressed.len(),
        entries.len(),
        entries.iter().map(|e| &e.entry.level).collect::<Vec<_>>(),
    );
}
