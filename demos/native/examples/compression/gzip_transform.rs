//! Streaming gzip compression as a `TransformStream`.
//!
//! Text chunks flow from a source into a stateful transformer that feeds them to a
//! `GzEncoder`, flushing after each chunk so compressed bytes are emitted incrementally
//! rather than all at the end. A file sink writes the compressed output, and `main`
//! reads it back and decompresses to confirm a clean round-trip.
//!
//! Run with: `cargo run --example gzip_transform`

use flate2::{Compression, write::GzEncoder};
use std::io::Write;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Yields chunks of text as bytes, then closes.
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
            controller.enqueue(self.chunks[self.index].as_bytes().to_vec())?;
            self.index += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Compresses incoming bytes, emitting compressed output as it becomes available.
#[derive(Default)]
pub struct GzipCompressor {
    encoder: Option<GzEncoder<Vec<u8>>>,
    emitted: usize,
}

impl Transformer<Vec<u8>, Vec<u8>> for GzipCompressor {
    async fn start(
        &mut self,
        _c: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.encoder = Some(GzEncoder::new(Vec::new(), Compression::default()));
        Ok(())
    }

    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let encoder = self.encoder.as_mut().expect("encoder set in start");
        encoder.write_all(&chunk)?;
        // Flush so the inner buffer holds everything compressed so far, then emit only
        // the bytes produced since the previous chunk.
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

/// Writes compressed bytes to a file.
pub struct CompressedFileWriter {
    path: String,
    file: Option<File>,
    bytes_written: usize,
}

impl CompressedFileWriter {
    pub fn new(path: &str) -> Self {
        Self {
            path: path.to_string(),
            file: None,
            bytes_written: 0,
        }
    }
}

impl WritableSink<Vec<u8>> for CompressedFileWriter {
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
            self.bytes_written += chunk.len();
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
    let output_file = "compressed_data.gz";
    let source = ReadableStream::builder(TextDataSource::sample_data()).spawn(tokio::spawn);
    let gzip = TransformStream::builder(GzipCompressor::default()).spawn(tokio::spawn);
    let sink = WritableStream::builder(CompressedFileWriter::new(output_file)).spawn(tokio::spawn);

    let compressed = source.pipe_through(gzip, None).spawn(tokio::spawn);
    compressed
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");

    let size = tokio::fs::metadata(output_file)
        .await
        .expect("stat output")
        .len();
    println!("wrote {output_file} ({size} bytes)");
    verify_roundtrip(output_file).await;
}

/// Reads the compressed file back and decompresses it to confirm integrity.
async fn verify_roundtrip(path: &str) {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let compressed = tokio::fs::read(path).await.expect("read output");
    let mut decoder = GzDecoder::new(&compressed[..]);
    let mut text = String::new();
    decoder.read_to_string(&mut text).expect("decompress");
    println!("round-trip ok: {} characters recovered", text.len());
}
