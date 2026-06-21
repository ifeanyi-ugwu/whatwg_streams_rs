//! Composing streams with `pipe_through` and `pipe_to`.
//!
//! `pipe_through` runs a readable through a transform and hands back the transform's
//! readable side; `pipe_to` drives a readable into a writable until it finishes.
//! Together they wire a source -> transform -> sink pipeline that runs to completion
//! in a single `await`, with backpressure handled for you.
//!
//! Run with: `cargo run --example piping`

use whatwg_streams::{
    ReadableStream, StreamResult, TransformStream, TransformStreamDefaultController, Transformer,
    WritableSink, WritableStream, WritableStreamDefaultController,
};

/// Doubles each number.
struct Double;

impl Transformer<u64, u64> for Double {
    async fn transform(
        &mut self,
        chunk: u64,
        controller: &mut TransformStreamDefaultController<u64>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk * 2)
    }
}

/// Prints each value it receives.
struct PrintSink;

impl WritableSink<u64> for PrintSink {
    async fn write(
        &mut self,
        chunk: u64,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("  got {chunk}");
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let source = ReadableStream::from_vec(vec![1u64, 2, 3, 4, 5]).spawn(tokio::spawn);
    let doubler = TransformStream::builder(Double).spawn(tokio::spawn);
    let sink = WritableStream::builder(PrintSink).spawn(tokio::spawn);

    println!("piping 1..=5 through a doubler into a printing sink:");

    // source -> doubler -> sink. pipe_through yields the doubled readable; pipe_to
    // drives it into the sink and resolves when the whole pipeline is done.
    let doubled = source.pipe_through(doubler, None).spawn(tokio::spawn);
    doubled.pipe_to(&sink, None).await.expect("pipe should complete");

    println!("done.");
}
