//! Simple Writable Example
//!
//! Basic writable stream example that:
//! - Implements a simple `WritableSink` that writes strings to the console
//! - Shows how to create a `WritableStream` from the sink
//! - Demonstrates writing multiple chunks and closing the stream

use futures::future::ready;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Simple sink that prints to the console
pub struct ConsoleSink;

impl WritableSink<String> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("🖨️ Writing chunk: {}", chunk);
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        println!("✅ ConsoleSink closed");
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("🚫 ConsoleSink aborted: {:?}", reason);
        Ok(())
    }
}

/// Example runner: write a few strings and close
pub async fn run_simple_writable_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Simple Writable Example ===");

    let sink = ConsoleSink;
    let writable =
        WritableStream::new_with_spawn(sink, Box::new(CountQueuingStrategy::new(1)), |fut| {
            tokio::spawn(fut);
        });

    let (_, writer) = writable.get_writer()?;

    writer.write("first line".to_string()).await?;
    writer.write("second line".to_string()).await?;
    writer.write("third line".to_string()).await?;

    writer.close().await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_simple_writable_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatwg_streams::dlc::ideation::d::writable_new::WritableStream;

    #[tokio::test]
    async fn test_simple_writable_stream() {
        let sink = ConsoleSink;
        let writable =
            WritableStream::new_with_spawn(sink, Box::new(CountQueuingStrategy::new(1)), |fut| {
                tokio::spawn(fut);
            });

        let (_, writer) = writable.get_writer().unwrap();

        writer.write("hello".to_string()).await.unwrap();
        writer.write("world".to_string()).await.unwrap();
        writer.close().await.unwrap();
    }
}
