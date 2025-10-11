//! Backpressure Demo Example
//!
//! Demonstrates how to handle backpressure in a streaming pipeline using
//! whatwg_streams. This example shows:
//! 1. A fast producer generating data
//! 2. A slow consumer processing data
//! 3. How a queue strategy controls memory usage and prevents overrun

use std::time::Duration;
use tokio::time::sleep;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Fast producer generating numbers
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
            println!("📦 Produced: {}", self.count);
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Slow consumer that simulates processing delay
pub struct SlowConsumer;

impl WritableSink<usize> for SlowConsumer {
    async fn write(
        &mut self,
        chunk: usize,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("📥 Consuming: {}", chunk);
        sleep(Duration::from_millis(200)).await; // simulate slow processing
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        println!("✅ Consumer done");
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Backpressure Demo ===");

    let producer = FastProducer::new(20);
    let readable = ReadableStream::new_default(producer, None);

    let consumer = SlowConsumer;
    let writable = WritableStream::new_with_spawn(
        consumer,
        Box::new(CountQueuingStrategy::new(5)), // queue allows up to 5 items before backpressure
        |fut| {
            tokio::spawn(fut);
        },
    );

    readable.pipe_to(&writable, None).await?;

    println!("✅ Backpressure demo completed");
    Ok(())
}
