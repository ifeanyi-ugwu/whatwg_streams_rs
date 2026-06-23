//! Streaming records out of an NDJSON file.
//!
//! The data is newline-delimited JSON — one object per line — the format you reach for
//! when records must be processed incrementally. The source reads the file in byte
//! chunks; a transform buffers them, slices out each complete line as it arrives, and
//! deserializes it into a `Person`; the sink prints each one. At no point is more than a
//! single line held in memory, so this scales to files far larger than RAM.
//!
//! A bracketed `[...]` JSON array cannot be streamed this way — the whole array is one
//! value and must be parsed at once. NDJSON exists precisely to make per-record
//! streaming possible.
//!
//! Run with: `cargo run --example json_stream`

use serde::Deserialize;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

#[derive(Deserialize)]
pub struct Person {
    name: String,
    age: u32,
}

/// Reads a file in fixed-size byte chunks, then closes.
pub struct NdjsonFileSource {
    file: File,
    buffer_size: usize,
}

impl NdjsonFileSource {
    pub async fn open(path: &str, buffer_size: usize) -> std::io::Result<Self> {
        Ok(Self {
            file: File::open(path).await?,
            buffer_size,
        })
    }
}

impl ReadableSource<Vec<u8>> for NdjsonFileSource {
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

/// Buffers bytes and deserializes each complete newline-terminated line into a `Person`.
#[derive(Default)]
pub struct PersonParser {
    buf: Vec<u8>,
}

impl PersonParser {
    fn parse_line(line: &[u8], controller: &mut TransformStreamDefaultController<Person>) -> StreamResult<()> {
        let text = std::str::from_utf8(line).map_err(|e| e.to_string())?.trim();
        if !text.is_empty() {
            let person: Person = serde_json::from_str(text).map_err(|e| e.to_string())?;
            controller.enqueue(person)?;
        }
        Ok(())
    }
}

impl Transformer<Vec<u8>, Person> for PersonParser {
    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Person>,
    ) -> StreamResult<()> {
        self.buf.extend_from_slice(&chunk);
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            Self::parse_line(&line, controller)?;
        }
        Ok(())
    }

    async fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<Person>,
    ) -> StreamResult<()> {
        // A final line with no trailing newline.
        let rest = std::mem::take(&mut self.buf);
        Self::parse_line(&rest, controller)
    }
}

/// Prints each parsed person.
pub struct ConsoleSink;

impl WritableSink<Person> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: Person,
        _c: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("  {} (age {})", chunk.name, chunk.age);
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let path = "people.ndjson";
    tokio::fs::write(
        path,
        "{\"name\":\"Alice\",\"age\":30}\n{\"name\":\"Bob\",\"age\":25}\n{\"name\":\"Charlie\",\"age\":35}\n",
    )
    .await
    .expect("write sample ndjson");

    // A deliberately small read buffer so lines genuinely split across chunks, exercising
    // the parser's cross-chunk buffering.
    let input = NdjsonFileSource::open(path, 16).await.expect("open ndjson");
    let source = ReadableStream::builder(input).spawn(tokio::spawn);
    let parser = TransformStream::builder(PersonParser::default()).spawn(tokio::spawn);
    let sink = WritableStream::builder(ConsoleSink).spawn(tokio::spawn);

    println!("streaming people from {path}:");
    let people = source.pipe_through(parser, None).spawn(tokio::spawn);
    people
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");
}
