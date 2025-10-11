//! Binary Protocol Parser Example
//!
//! Demonstrates a simple framed protocol parser as a TransformStream:
//! - frames are length-prefixed: 4 bytes (big-endian u32) length + payload
//! - the transformer accumulates partial chunks and emits complete payloads
//!
//! This mimics real-world parsing for TCP/binary protocols where frames can
//! be split across chunk boundaries.

use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    transform::{TransformStream, TransformStreamDefaultController, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A test source that yields Vec<u8> chunks (possibly splitting frames).
pub struct SampleChunkSource {
    chunks: Vec<Vec<u8>>,
    i: usize,
}

impl SampleChunkSource {
    pub fn new(chunks: Vec<Vec<u8>>) -> Self {
        Self { chunks, i: 0 }
    }
}

impl ReadableSource<Vec<u8>> for SampleChunkSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        if self.i < self.chunks.len() {
            controller.enqueue(self.chunks[self.i].clone())?;
            self.i += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Transformer that parses length-prefixed frames and emits payloads (Vec<u8>).
pub struct FrameParser {
    buf: Vec<u8>, // accumulated bytes
}

impl FrameParser {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }
}

impl Transformer<Vec<u8>, Vec<u8>> for FrameParser {
    async fn start(
        &mut self,
        _controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.buf.clear();
        Ok(())
    }

    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        // append incoming chunk
        self.buf.extend_from_slice(&chunk);

        // Try to parse as many complete frames as possible
        loop {
            if self.buf.len() < 4 {
                // not enough bytes for length
                break;
            }

            // read 4-byte big-endian length
            let len = {
                let mut arr = [0u8; 4];
                arr.copy_from_slice(&self.buf[0..4]);
                u32::from_be_bytes(arr) as usize
            };

            if self.buf.len() < 4 + len {
                // not enough bytes for the full payload
                break;
            }

            // Extract payload
            let payload = self.buf[4..4 + len].to_vec();
            controller.enqueue(payload)?;

            // Remove parsed frame from buffer
            self.buf.drain(0..4 + len);
        }

        Ok(())
    }

    async fn flush(
        &mut self,
        _controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.buf.clear();
        Ok(())
    }
}

/// A simple writable sink that prints each parsed frame
pub struct FramePrinter;

impl WritableSink<Vec<u8>> for FramePrinter {
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("📦 Received frame ({} bytes): {:?}", chunk.len(), chunk);
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        println!("✅ FramePrinter closed");
        Ok(())
    }
}

/// Example function demonstrating the binary protocol parser
pub async fn run_binary_protocol_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Binary Protocol Parser Example ===");

    // Sample data: 2 frames (length-prefixed)
    let chunks = vec![
        vec![0, 0, 0, 3, 1, 2],    // first frame partially
        vec![3, 0, 0, 0, 2, 9, 8], // rest of first + second frame
    ];

    let readable = ReadableStream::new_default(SampleChunkSource::new(chunks), None);

    let transform = TransformStream::new(FrameParser::new());

    let sink = WritableStream::new_with_spawn(
        FramePrinter,
        Box::new(CountQueuingStrategy::new(2)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    readable
        .pipe_through(transform, None)
        .pipe_to(&sink, None)
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_binary_protocol_example().await
}
