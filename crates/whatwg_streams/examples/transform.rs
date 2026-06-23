//! Implementing a `TransformStream`.
//!
//! A transform stream has two halves: you write into its writable side, and the
//! transformed chunks come out of its readable side. This transform is stateful
//! (it numbers the lines) and uses `flush` to emit a trailer once the input closes.
//!
//! The two halves run concurrently — the producer writes on its own task while the
//! consumer reads here. That is the natural shape for a transform, and it is also
//! required: with the default readable high-water mark of 0, a `write` resolves only
//! once the reader takes the chunk, so producing and consuming must overlap.
//!
//! Run with: `cargo run --example transform`

use whatwg_streams::{StreamResult, TransformStream, TransformStreamDefaultController, Transformer};

/// Numbers and upcases each line; emits a `(N lines)` trailer on close.
struct NumberLines {
    count: usize,
}

impl Transformer<String, String> for NumberLines {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        self.count += 1;
        controller.enqueue(format!("{}: {}", self.count, chunk.to_uppercase()))
    }

    async fn flush(
        &mut self,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        // `flush` runs after the last write, just before the readable side closes.
        controller.enqueue(format!("({} lines)", self.count))?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let transform = TransformStream::builder(NumberLines { count: 0 }).spawn(tokio::spawn);
    let (readable, writable) = transform.split();

    // Producer: feed lines into the writable side on its own task, then close.
    let (_wlock, writer) = writable.get_writer().expect("a fresh stream is unlocked");
    let producer = tokio::spawn(async move {
        for line in ["hello", "from", "a transform"] {
            writer.write(line.to_string()).await.expect("write should not error");
        }
        writer.close().await.expect("close should not error");
    });

    // Consumer: read the transformed output from the readable side.
    let (_rlock, reader) = readable.get_reader().expect("a fresh stream is unlocked");
    while let Some(out) = reader.read().await.expect("read should not error") {
        println!("{out}");
    }

    producer.await.expect("producer task panicked");
}
