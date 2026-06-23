//! A custom `WritableStream` sink.
//!
//! `ConsoleSink` consumes each written chunk by printing it; `close` runs once the
//! writer is done. A `write` resolving is where backpressure would apply if the sink
//! were slow to accept the chunk.
//!
//! Run with: `cargo run --example simple_writable`

use whatwg_streams::{
    StreamResult, WritableSink, WritableStream, WritableStreamDefaultController,
};

/// Sink that prints each chunk to stdout.
pub struct ConsoleSink;

impl WritableSink<String> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("  writing: {chunk}");
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        println!("sink closed.");
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let stream = WritableStream::builder(ConsoleSink).spawn(tokio::spawn);
    let (_lock, writer) = stream.get_writer().expect("a fresh stream is unlocked");

    for line in ["first line", "second line", "third line"] {
        writer
            .write(line.to_string())
            .await
            .expect("write should not error");
    }
    writer.close().await.expect("close should not error");
}
