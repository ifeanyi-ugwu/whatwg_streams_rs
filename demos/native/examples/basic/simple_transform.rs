//! Simple Transform Example
//!
//! Demonstrates a basic transform stream pipeline:
//! - A readable source yields lowercase words
//! - A transform stream converts them to uppercase
//! - A writable sink prints the transformed output
//!
//! Shows the core building blocks for stream pipelines.

use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    transform::{TransformStream, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A simple readable source that yields some lowercase words
pub struct WordSource {
    words: Vec<String>,
    idx: usize,
}

impl WordSource {
    pub fn new() -> Self {
        Self {
            words: vec!["alpha".into(), "bravo".into(), "charlie".into()],
            idx: 0,
        }
    }
}

impl ReadableSource<String> for WordSource {
    async fn start(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        println!("✅ WordSource started");
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.idx < self.words.len() {
            let word = self.words[self.idx].clone();
            self.idx += 1;
            controller.enqueue(word)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("🚫 WordSource cancelled: {:?}", reason);
        Ok(())
    }
}

/// Transformer that converts strings to uppercase
pub struct UppercaseTransformer;

impl Transformer<String, String> for UppercaseTransformer {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut whatwg_streams::dlc::ideation::d::transform::TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        let upper = chunk.to_uppercase();
        controller.enqueue(upper)?;
        Ok(())
    }
}

/// Writable sink that prints each chunk
pub struct ConsoleSink;

impl WritableSink<String> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("🖨️ OUT: {}", chunk);
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

/// Example runner: pipe readable → transform → writable
pub async fn run_simple_transform_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Simple Transform Example ===");

    // Build readable stream from WordSource
    let readable = ReadableStream::new_default(WordSource::new(), None);

    // Create transform stream
    let transform = TransformStream::new(UppercaseTransformer);

    // Create writable sink
    let writable = WritableStream::new_with_spawn(
        ConsoleSink,
        Box::new(CountQueuingStrategy::new(1)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    // Pipe them: readable → transform → writable
    readable
        .pipe_through(transform, None)
        .pipe_to(&writable, None)
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_simple_transform_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatwg_streams::dlc::ideation::d::{
        transform::TransformStream, writable_new::WritableStream,
    };

    struct CollectSink {
        items: std::sync::Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl WritableSink<String> for CollectSink {
        async fn write(
            &mut self,
            chunk: String,
            _controller: &mut WritableStreamDefaultController,
        ) -> StreamResult<()> {
            self.items.lock().await.push(chunk);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_simple_transform_pipeline() {
        let readable = ReadableStream::builder(WordSource::new()).build();
        let transform = TransformStream::new(UppercaseTransformer);

        let items = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sink = CollectSink {
            items: items.clone(),
        };

        let writable =
            WritableStream::new_with_spawn(sink, Box::new(CountQueuingStrategy::new(1)), |fut| {
                tokio::spawn(fut);
            });

        readable
            .pipe_through(transform, None)
            .pipe_to(&writable, None)
            .await
            .unwrap();

        let collected = items.lock().await.clone();
        assert_eq!(collected, vec!["ALPHA", "BRAVO", "CHARLIE"]);
    }
}
