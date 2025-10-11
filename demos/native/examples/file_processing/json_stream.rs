//! JSON Stream Example
//!
//! Demonstrates how to process JSON data incrementally as a stream.
//! - Reads a JSON array from a source
//! - Parses objects one by one using serde_json
//! - Prints each object as it is processed
//!
//! This is memory-efficient for large JSON arrays.

use serde::Deserialize;
use serde_json::Deserializer;
use tokio::fs::File;
use tokio::io::{self, AsyncReadExt};
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Example struct to deserialize JSON objects into
#[derive(Debug, Deserialize, Clone, PartialEq)]
struct Person {
    name: String,
    age: u32,
}

/// Source that streams JSON objects from a file
pub struct JsonFileSource {
    data: Vec<u8>,
    cursor: usize,
    objects: Vec<Person>,
    idx: usize,
}

impl JsonFileSource {
    pub async fn from_file(path: &str) -> io::Result<Self> {
        let mut file = File::open(path).await?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;

        // Deserialize JSON array into Vec<Person>
        let objects: Vec<Person> = serde_json::from_slice(&buf)?;

        Ok(Self {
            data: buf,
            cursor: 0,
            objects,
            idx: 0,
        })
    }
}

impl ReadableSource<Person> for JsonFileSource {
    async fn start(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<Person>,
    ) -> StreamResult<()> {
        println!("✅ JSON stream started");
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Person>,
    ) -> StreamResult<()> {
        if self.idx < self.objects.len() {
            let obj = self.objects[self.idx].clone();
            self.idx += 1;
            controller.enqueue(obj)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("🚫 JSON stream cancelled: {:?}", reason);
        Ok(())
    }
}

/// Writable sink that prints JSON objects
pub struct ConsoleSink;

impl WritableSink<Person> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: Person,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("👤 Person: {:?}", chunk);
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

/// Example runner
pub async fn run_json_stream_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== JSON Stream Example ===");

    // Create a small JSON test file
    let path = "examples/file_processing/people.json";
    tokio::fs::write(
        path,
        r#"[{"name":"Alice","age":30},{"name":"Bob","age":25},{"name":"Charlie","age":35}]"#,
    )
    .await?;

    // Build JSON stream
    let source = JsonFileSource::from_file(path).await?;
    let readable = ReadableStream::new_default(source, None);

    // Build writable
    let writable = WritableStream::new_with_spawn(
        ConsoleSink,
        Box::new(CountQueuingStrategy::new(1)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    // Pipe JSON stream to console
    readable.pipe_to(&writable, None).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_json_stream_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct CollectSink {
        items: Arc<Mutex<Vec<Person>>>,
    }

    impl WritableSink<Person> for CollectSink {
        async fn write(
            &mut self,
            chunk: Person,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.items.lock().await.push(chunk);
            Ok(())
        }

        async fn close(self) -> StreamResult<()> {
            Ok(())
        }

        async fn abort(&mut self, _reason: Option<String>) -> StreamResult<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_json_stream_collects_people() {
        let path = "examples/file_processing/test_people.json";
        tokio::fs::write(
            path,
            r#"[{"name":"Dana","age":40},{"name":"Eli","age":28}]"#,
        )
        .await
        .unwrap();

        let source = JsonFileSource::from_file(path).await.unwrap();
        let readable = ReadableStream::new_default(source, None);

        let items = Arc::new(Mutex::new(Vec::new()));
        let sink = CollectSink {
            items: items.clone(),
        };

        let writable =
            WritableStream::new_with_spawn(sink, Box::new(CountQueuingStrategy::new(1)), |fut| {
                tokio::spawn(fut);
            });

        readable.pipe_to(&writable, None).await.unwrap();

        let collected = items.lock().await.clone();
        assert_eq!(
            collected,
            vec![
                Person {
                    name: "Dana".into(),
                    age: 40
                },
                Person {
                    name: "Eli".into(),
                    age: 28
                }
            ]
        );
    }
}
