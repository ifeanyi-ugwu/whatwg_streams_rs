//! Backpressure across a pipeline, made visible.
//!
//! A fast producer feeds a slow sink through a queue with a high-water mark of 5.
//! Each `pull` prints the controller's remaining `desired_size`: it counts down as the
//! queue fills, reaches 0, and the producer then stalls until the sink drains a slot.
//! The producer never races more than ~5 items ahead of the sink — that is backpressure.
//!
//! Run with: `cargo run --example backpressure_demo`

use std::time::Duration;
use tokio::time::sleep;
use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamResult, WritableSink, WritableStream, WritableStreamDefaultController,
};

/// Produces 1..=max as fast as the queue will accept.
pub struct FastProducer {
    count: usize,
    max: usize,
}

impl FastProducer {
    pub fn new(max: usize) -> Self {
        Self { count: 0, max }
    }
}

impl ReadableSource<usize> for FastProducer {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<usize>,
    ) -> StreamResult<()> {
        if self.count < self.max {
            self.count += 1;
            controller.enqueue(self.count)?;
            println!(
                "produced {:>2}  (queue room left: {:?})",
                self.count,
                controller.desired_size()
            );
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Accepts one item every 200ms.
pub struct SlowConsumer;

impl WritableSink<usize> for SlowConsumer {
    async fn write(
        &mut self,
        chunk: usize,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("        consumed {chunk:>2}");
        sleep(Duration::from_millis(200)).await;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let source = ReadableStream::builder(FastProducer::new(20))
        .strategy(CountQueuingStrategy::new(5))
        .spawn(tokio::spawn);
    let sink = WritableStream::builder(SlowConsumer).spawn(tokio::spawn);

    source
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");
}
