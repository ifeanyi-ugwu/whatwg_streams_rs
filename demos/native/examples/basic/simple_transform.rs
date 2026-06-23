//! Composing a readable, a transform, and a writable into one pipeline.
//!
//! Lowercase words leave a source, a transform upcases them, and a sink prints the
//! result. `pipe_through` runs the source through the transform and hands back the
//! transform's readable side; `pipe_to` then drives that into the sink to completion,
//! applying backpressure across the whole chain.
//!
//! Run with: `cargo run --example simple_transform`

use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Yields a fixed list of lowercase words, then closes.
pub struct WordSource {
    words: Vec<String>,
    idx: usize,
}

impl Default for WordSource {
    fn default() -> Self {
        Self {
            words: vec!["alpha".into(), "bravo".into(), "charlie".into()],
            idx: 0,
        }
    }
}

impl ReadableSource<String> for WordSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.idx < self.words.len() {
            controller.enqueue(self.words[self.idx].clone())?;
            self.idx += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Upcases each chunk.
pub struct UppercaseTransformer;

impl Transformer<String, String> for UppercaseTransformer {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk.to_uppercase())
    }
}

/// Prints each chunk it receives.
pub struct ConsoleSink;

impl WritableSink<String> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("  {chunk}");
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let source = ReadableStream::builder(WordSource::default()).spawn(tokio::spawn);
    let transform = TransformStream::builder(UppercaseTransformer).spawn(tokio::spawn);
    let sink = WritableStream::builder(ConsoleSink).spawn(tokio::spawn);

    println!("source -> uppercase -> sink:");
    let upper = source.pipe_through(transform, None).spawn(tokio::spawn);
    upper.pipe_to(&sink, None).await.expect("pipe should complete");
}
