//! Simple Readable Example
//!
//! Basic readable stream example that:
//! - Implements a small `ReadableSource` which yields lines from a Vec<String>
//! - Shows how to create a `ReadableStream` from the source and read from it
//! - Demonstrates proper closing and a simple test

use futures::future::ready;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
};

/// Simple in-memory source that yields lines
pub struct SimpleReadableSource {
    items: Vec<String>,
    idx: usize,
}

impl SimpleReadableSource {
    pub fn new(items: Vec<&str>) -> Self {
        Self {
            items: items.into_iter().map(String::from).collect(),
            idx: 0,
        }
    }
}

impl ReadableSource<String> for SimpleReadableSource {
    async fn start(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        // no-op start
        println!("✅ SimpleReadableSource started");
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.idx < self.items.len() {
            let item = self.items[self.idx].clone();
            self.idx += 1;
            controller.enqueue(item)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("🚫 SimpleReadableSource cancelled: {:?}", reason);
        Ok(())
    }
}

/// Example runner: create stream, read all items, print them
pub async fn run_simple_readable_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Simple Readable Example ===");

    let items = vec!["alpha", "bravo", "charlie"];
    let source = SimpleReadableSource::new(items);

    // Create the readable stream from the source
    let readable = ReadableStream::new_default(source, None);

    // Get the default (locked) reader
    let reader = readable.get_reader().1;

    // Read until the stream closes
    loop {
        match reader.read().await? {
            Some(chunk) => {
                println!("📥 got chunk: {}", chunk);
            }
            None => {
                println!("📭 stream closed");
                break;
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_simple_readable_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatwg_streams::dlc::ideation::d::readable_sampling_b::ReadableStream;

    #[tokio::test]
    async fn test_simple_readable_stream() {
        let items = vec!["one", "two", "three"];
        let source = SimpleReadableSource::new(items);
        let readable = ReadableStream::new_default(source, None);
        let reader = readable.get_reader().1;

        let mut seen = Vec::new();
        loop {
            match reader.read().await.unwrap() {
                Some(s) => seen.push(s),
                None => break,
            }
        }

        assert_eq!(seen, vec!["one", "two", "three"]);
    }
}
