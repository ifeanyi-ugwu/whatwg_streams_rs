//! Filtering a stream and writing the survivors to a file.
//!
//! Log lines flow from an in-memory source, a transform drops everything that is not
//! an `ERROR`, and a file sink appends each surviving line. The transform is the
//! canonical stateless filter: emit the chunk, or emit nothing.
//!
//! Run with: `cargo run --example log_filter`

use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Yields log lines from memory, then closes.
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
            controller.enqueue(self.lines[self.index].clone())?;
            self.index += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Passes through only lines containing `ERROR`.
pub struct ErrorFilter;

impl Transformer<String, String> for ErrorFilter {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if chunk.contains("ERROR") {
            controller.enqueue(chunk)?;
        }
        Ok(())
    }
}

/// Appends each chunk as a line, opening the file on the first write.
pub struct FileWriterSink {
    file_path: String,
    file: Option<File>,
}

impl FileWriterSink {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file: None,
        }
    }
}

impl WritableSink<String> for FileWriterSink {
    async fn start(&mut self, _c: &mut WritableStreamDefaultController) -> StreamResult<()> {
        self.file = Some(File::create(&self.file_path).await?);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: String,
        _c: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file {
            file.write_all(chunk.as_bytes()).await?;
            file.write_all(b"\n").await?;
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
    let log_data = vec![
        "2024-01-01 10:00:00 INFO: Application started successfully",
        "2024-01-01 10:05:23 ERROR: User authentication failed for user 'alice'",
        "2024-01-01 10:07:45 DEBUG: Database connection pool initialized",
        "2024-01-01 10:12:33 ERROR: Database connection timeout after 30 seconds",
        "2024-01-01 10:15:22 WARN: Memory usage is at 85%",
        "2024-01-01 10:18:44 ERROR: File not found: /tmp/missing_file.txt",
        "2024-01-01 10:20:11 INFO: Background job completed successfully",
    ];

    let output_path = "filtered_errors.log";
    let source = ReadableStream::builder(LogFileSource::new(log_data)).spawn(tokio::spawn);
    let filter = TransformStream::builder(ErrorFilter).spawn(tokio::spawn);
    let sink = WritableStream::builder(FileWriterSink::new(output_path)).spawn(tokio::spawn);

    let errors = source.pipe_through(filter, None).spawn(tokio::spawn);
    errors
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");

    let content = tokio::fs::read_to_string(output_path)
        .await
        .expect("read back output");
    println!("wrote {} ERROR lines to {output_path}:", content.lines().count());
    print!("{content}");
}
