//! Implementing a custom `WritableStream` sink.
//!
//! A sink consumes data: its `write` method is called for each chunk, `close` is
//! called once the writer is done, and `abort` is called if the stream is torn
//! down early. Note `close(self)` takes the sink *by value* — it consumes the sink,
//! which is convenient when finishing means turning accumulated state into a final
//! result (here, summarizing the collected lines).
//!
//! Run with: `cargo run --example writable_sink`

use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

/// Collects each written line, and on close reports what it gathered.
struct LogSink {
    lines: Vec<String>,
}

impl WritableSink<String> for LogSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("  sink received: {chunk:?}");
        self.lines.push(chunk);
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        let bytes: usize = self.lines.iter().map(String::len).sum();
        println!("sink closed: {} lines, {bytes} bytes total", self.lines.len());
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("sink aborted: {}", reason.unwrap_or_else(|| "<no reason>".into()));
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let stream = WritableStream::builder(LogSink { lines: Vec::new() }).spawn(tokio::spawn);

    // A writer locks the stream for exclusive production.
    let (_lock, writer) = stream.get_writer().expect("a fresh stream is unlocked");

    for line in ["hello", "from", "whatwg_streams"] {
        // `write` resolves once the sink has processed the chunk — this is where
        // backpressure is applied if the sink is slow.
        writer.write(line.to_string()).await.expect("write should not error");
    }

    // Closing drains any pending writes, then calls the sink's `close`.
    writer.close().await.expect("close should not error");
}
