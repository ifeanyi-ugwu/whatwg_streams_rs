//! A length-prefixed frame parser as a `TransformStream`.
//!
//! Frames are `[u32 big-endian length][payload]`. Input chunks may split or merge
//! frames arbitrarily, so the transformer buffers incoming bytes and emits each
//! complete payload as soon as it has fully arrived — the shape of most TCP/binary
//! protocol parsers.
//!
//! Run with: `cargo run --example binary_protocol`

use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Yields raw byte chunks, deliberately splitting frames across chunk boundaries.
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

/// Buffers bytes and emits one payload per complete `[len][payload]` frame.
#[derive(Default)]
pub struct FrameParser {
    buf: Vec<u8>,
}

impl Transformer<Vec<u8>, Vec<u8>> for FrameParser {
    async fn transform(
        &mut self,
        chunk: Vec<u8>,
        controller: &mut TransformStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        self.buf.extend_from_slice(&chunk);

        // Emit every frame the buffer now holds in full; stop at the first partial one.
        while self.buf.len() >= 4 {
            let len = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
            if self.buf.len() < 4 + len {
                break;
            }
            let payload = self.buf[4..4 + len].to_vec();
            controller.enqueue(payload)?;
            self.buf.drain(0..4 + len);
        }
        Ok(())
    }
}

/// Prints each parsed frame.
pub struct FramePrinter;

impl WritableSink<Vec<u8>> for FramePrinter {
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("  frame ({} bytes): {:?}", chunk.len(), chunk);
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    // Two frames (payloads [1,2,3] and [9,8]), chopped across chunk boundaries.
    let chunks = vec![
        vec![0, 0, 0, 3, 1, 2],    // frame-1 length + first 2 payload bytes
        vec![3, 0, 0, 0, 2, 9, 8], // last payload byte of frame-1, then frame-2
    ];

    let source = ReadableStream::builder(SampleChunkSource::new(chunks)).spawn(tokio::spawn);
    let parser = TransformStream::builder(FrameParser::default()).spawn(tokio::spawn);
    let sink = WritableStream::builder(FramePrinter).spawn(tokio::spawn);

    println!("parsing length-prefixed frames:");
    let frames = source.pipe_through(parser, None).spawn(tokio::spawn);
    frames
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");
}
