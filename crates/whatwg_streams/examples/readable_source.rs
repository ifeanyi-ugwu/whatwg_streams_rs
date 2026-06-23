//! Implementing a custom `ReadableStream` source.
//!
//! A source produces data on demand: its `pull` method is called whenever the
//! stream needs another chunk, and it either `enqueue`s a value or `close`s the
//! stream. Because `pull` runs only as the consumer reads, the sequence is lazy.
//!
//! Run with: `cargo run --example readable_source`

use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult,
};

/// Yields the squares 1², 2², … up to `count`, then closes the stream.
struct Squares {
    next: u64,
    count: u64,
}

impl ReadableSource<u64> for Squares {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u64>,
    ) -> StreamResult<()> {
        if self.next > self.count {
            // No more data — closing makes the next `read()` resolve to `None`.
            controller.close()?;
        } else {
            controller.enqueue(self.next * self.next)?;
            self.next += 1;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    // `spawn` drives the stream's machinery on the given runtime; the source's
    // `pull` is invoked on demand as the reader below consumes.
    let stream = ReadableStream::builder(Squares { next: 1, count: 8 }).spawn(tokio::spawn);

    // A reader locks the stream for exclusive consumption.
    let (_lock, reader) = stream.get_reader().expect("a fresh stream is unlocked");

    println!("first eight squares:");
    while let Some(n) = reader.read().await.expect("read should not error") {
        println!("  {n}");
    }
    println!("stream closed.");
}
