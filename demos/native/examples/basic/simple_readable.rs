//! A custom `ReadableStream` source.
//!
//! `SimpleReadableSource` yields lines from a `Vec<String>`: each `pull` enqueues the
//! next item, and once they run out it closes the stream so the reader sees `None`.
//!
//! Run with: `cargo run --example simple_readable`

use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult,
};

/// In-memory source that yields each stored line once, then closes.
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
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.idx < self.items.len() {
            controller.enqueue(self.items[self.idx].clone())?;
            self.idx += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let source = SimpleReadableSource::new(vec!["alpha", "bravo", "charlie"]);
    let stream = ReadableStream::builder(source).spawn(tokio::spawn);

    let (_lock, reader) = stream.get_reader().expect("a fresh stream is unlocked");

    println!("reading until the stream closes:");
    while let Some(chunk) = reader.read().await.expect("read should not error") {
        println!("  {chunk}");
    }
    println!("stream closed.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn yields_each_item_then_closes() {
        let source = SimpleReadableSource::new(vec!["one", "two", "three"]);
        let stream = ReadableStream::builder(source).spawn(tokio::spawn);
        let (_lock, reader) = stream.get_reader().unwrap();

        let mut seen = Vec::new();
        while let Some(s) = reader.read().await.unwrap() {
            seen.push(s);
        }
        assert_eq!(seen, vec!["one", "two", "three"]);
    }
}
