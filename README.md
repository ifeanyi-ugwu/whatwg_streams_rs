# whatwg_streams

A high-performance, WHATWG Streams API-compliant implementation for Rust, providing ReadableStream, WritableStream, and TransformStream primitives with full backpressure support.

[![Crates.io](https://img.shields.io/crates/v/whatwg_streams.svg)](https://crates.io/crates/whatwg_streams)
[![Documentation](https://docs.rs/whatwg_streams/badge.svg)](https://docs.rs/whatwg_streams)

This crate mirrors the browser Streams API while adapting to Rust's ownership model and async ecosystem. It provides built-in flow control to prevent memory exhaustion, zero-copy operations for efficiency, and type-safe locking to prevent reader/writer conflicts at compile time.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
whatwg_streams = "0.1.0"
```

## Runtime Agnostic

This crate is **not tied to any specific async runtime**.  
When you call `.spawn(...)`, you provide a function or closure that schedules/runs background tasks in the runtime you choose.

For example:

### Tokio

```rust
let stream = ReadableStream::from_vec(vec![1, 2, 3])
    .spawn(tokio::task::spawn);
```

### async-std

```rust
let stream = ReadableStream::from_vec(vec![1, 2, 3])
    .spawn(async_std::task::spawn);
```

### smol

```rust
let stream = ReadableStream::from_vec(vec![1, 2, 3])
    .spawn(smol::spawn);
```

### Custom executor

You can also supply your own executor/spawner.
Here is how you might use `futures::executor::LocalPool`:

```rust
use futures::executor::LocalPool;
use futures::task::LocalSpawnExt;

let mut pool = LocalPool::new();
let spawner = pool.spawner();

// Drive your application code on the pool
pool.run_until(async move {
    let stream = ReadableStream::from_vec(vec![10, 20, 30])
        .spawn(|fut| spawner.spawn_local(fut).unwrap());

    let (_, reader) = stream.get_reader().unwrap();

    let mut got = Vec::new();
    while let Some(item) = reader.read().await.expect("read failed") {
        got.push(item);
    }

    assert_eq!(got, vec![10, 20, 30]);

    // you can run other async tasks here too
});
```

### Raw Thread Execution

Streams are fully runtime-agnostic: `.spawn(...)` accepts any spawner that drives a future.
This means you can even run a stream on a raw thread without a full async runtime:

```rust
use futures::executor::block_on;
use whatwg_streams::ReadableStream;

// Each future is driven to completion inside a separate thread
let stream = ReadableStream::from_vec(vec![1, 2, 3])
    .spawn(|fut| {
        std::thread::spawn(move || {
            block_on(fut);
        });
    });
```

This approach is useful for lightweight single-use threads or environments where you don’t want a full async runtime.
For most applications, using a proper runtime like Tokio, async-std, or Smol remains recommended.

## Local Streams (non-Send)

In addition to the default `send`-based API, this crate also provides a **`local`** module.
It offers the exact same `ReadableStream`, `WritableStream`, and `TransformStream` types, but optimized for **single-threaded runtimes**:

- Internally uses `Rc` instead of `Arc`.
- Futures are **not required to be `Send`**.
- Slightly lower overhead if you don’t need multi-threaded execution.

This is useful if you’re using executors like `tokio::task::spawn_local` or `futures::executor::LocalPool`.

```rust
use whatwg_streams::local::ReadableStream;
use futures::StreamExt;
use tokio::task::LocalSet;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let local = LocalSet::new();

    local.run_until(async {
        let stream = ReadableStream::from_vec(vec![1, 2, 3])
            .spawn(tokio::task::spawn_local);

        let (_, mut reader) = stream.get_reader().unwrap();
        let mut collected = Vec::new();

        while let Some(item) = reader.read().await.unwrap() {
            collected.push(item);
        }

        assert_eq!(collected, vec![1, 2, 3]);
        println!("Local stream read successfully!");
    }).await;
}
```

Both `send` and `local` modules expose the same API surface.
Choose **`send`** (default) if you want multi-threaded flexibility,
or **`local`** if you’re sure everything runs on a single thread.

### Basic Usage

```rust
use whatwg_streams::ReadableStream;
use futures::StreamExt;

// Create a stream from an iterator
let data = vec![1, 2, 3, 4, 5];
let stream = ReadableStream::from_iterator(data.into_iter())
    .spawn(tokio::task::spawn);

let (_, reader) = stream.get_reader().unwrap();

// Read values
while let Some(value) = reader.read().await.unwrap() {
    println!("Got: {}", value);
}
```

### Writable Streams

```rust
use whatwg_streams::{WritableStream, WritableSink, error::StreamError};

#[derive(Clone)]
struct ConsoleSink;

impl WritableSink<String> for ConsoleSink {
    async fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> Result<(), StreamError> {
        println!("{}", chunk);
        Ok(())
    }
}

let stream = WritableStream::builder(ConsoleSink)
    .spawn(tokio::task::spawn);

let (_, writer) = stream.get_writer().unwrap();
writer.write("Hello, World!".to_string()).await.unwrap();
writer.close().await.unwrap();
```

### Transform Streams

```rust
use whatwg_streams::{TransformStream, Transformer, TransformStreamDefaultController};

struct UppercaseTransformer;

impl Transformer<String, String> for UppercaseTransformer {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> Result<(), StreamError> {
        controller.enqueue(chunk.to_uppercase())
    }
}

let source = ReadableStream::from_vec(vec!["hello", "world"])
    .spawn(tokio::task::spawn);

let transform = TransformStream::builder(UppercaseTransformer)
    .spawn(tokio::task::spawn);

let output = source.pipe_through(transform, None)
    .spawn(tokio::task::spawn);

let (_, reader) = output.get_reader().unwrap();
assert_eq!(reader.read().await.unwrap(), Some("HELLO".to_string()));
assert_eq!(reader.read().await.unwrap(), Some("WORLD".to_string()));
```

## Core Concepts

### ReadableStream

Represents a source of data that can be read chunk-by-chunk:

```rust
// From various sources
let stream1 = ReadableStream::from_vec(vec![1, 2, 3])
    .spawn(tokio::task::spawn);

let stream2 = ReadableStream::from_iterator(0..100)
    .spawn(tokio::task::spawn);

let async_stream = futures::stream::iter(vec!["a", "b", "c"]);
let stream3 = ReadableStream::from_stream(async_stream)
    .spawn(tokio::task::spawn);
```

### WritableStream

Represents a destination that accepts data:

```rust
#[derive(Clone)]
struct FileSink {
    path: PathBuf,
}

impl WritableSink<Vec<u8>> for FileSink {
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> Result<(), StreamError> {
        tokio::fs::write(&self.path, chunk).await?;
        Ok(())
    }
}

let sink = FileSink { path: "output.txt".into() };
let stream = WritableStream::builder(sink)
    .strategy(CountQueuingStrategy::new(10)) // Buffer up to 10 chunks
    .spawn(tokio::task::spawn);
```

### Backpressure

Streams automatically handle backpressure to prevent memory issues:

```rust
let (_, writer) = writable_stream.get_writer().unwrap();

// Sequential writes - each write waits for completion
writer.write(data1).await?;
writer.write(data2).await?;

// For high throughput without waiting for completion:
// Check if ready first, then enqueue without waiting
writer.ready().await?;
writer.enqueue(data3)?; // Enqueues immediately, doesn't wait

// Or use the helper that waits for readiness
writer.enqueue_when_ready(data4).await?; // Waits for ready, then enqueues
```

### Byte Streams

Optimized for binary data with zero-copy operations:

```rust
use whatwg_streams::ReadableByteSource;

struct FileByteSource {
    file: tokio::fs::File,
}

impl ReadableByteSource for FileByteSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> Result<usize, StreamError> {
        let bytes_read = self.file.read(buffer).await?;
        if bytes_read == 0 {
            controller.close()?;
        }
        Ok(bytes_read)
    }
}

let stream = ReadableStream::builder_bytes(source)
    .spawn(tokio::task::spawn);

// BYOB reader for zero-copy reads
let (_, reader) = stream.get_byob_reader().unwrap();
let mut buffer = [0u8; 1024];
let bytes_read = reader.read(&mut buffer).await?;
```

## Advanced Features

### Stream Teeing

Split a stream into multiple independent branches:

```rust
let source = ReadableStream::from_vec(vec![1, 2, 3])
    .spawn(tokio::task::spawn_local);

let (stream1, stream2) = source
    .tee()
    .backpressure_mode(BackpressureMode::SpecCompliant)
    .spawn(tokio::task::spawn)?;

// Both streams receive the same data
```

### Piping

Connect readable and writable streams:

```rust
source_stream.pipe_to(&destination_stream, None).await?;

// With options
use futures::future::AbortRegistration;

let (abort_handle, registration) = AbortRegistration::new();
let options = StreamPipeOptions {
    prevent_close: false,
    prevent_abort: false,
    prevent_cancel: false,
    signal: Some(registration),
};

source_stream.pipe_to(&destination_stream, Some(options)).await?;
```

### Custom Queuing Strategies

Control buffering behavior:

```rust
use whatwg_streams::CountQueuingStrategy;

struct CustomStrategy {
    max_size: usize,
}

impl QueuingStrategy<MyData> for CustomStrategy {
    fn size(&self, chunk: &MyData) -> usize {
        chunk.byte_length()
    }

    fn high_water_mark(&self) -> usize {
        self.max_size
    }
}

let stream = ReadableStream::builder(source)
    .strategy(CustomStrategy { max_size: 1024 })
    .spawn(tokio::task::spawn);
```

## Error Handling

Streams provide comprehensive error handling:

```rust
use whatwg_streams::error::StreamError;

// Errors propagate through the stream
match reader.read().await {
    Ok(Some(data)) => process(data),
    Ok(None) => println!("Stream ended"),
    Err(StreamError::Canceled) => println!("Operation was canceled"),
    Err(StreamError::Aborted(reason)) => println!("Stream aborted: {:?}", reason),
    Err(StreamError::Closed) => println!("Stream is closed"),
    Err(StreamError::Other(err)) => println!("Other error: {}", err),
}
```

## Contributing

Contributions are welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

Licensed under the MIT License.

## Acknowledgments

This implementation follows the [WHATWG Streams Standard](https://streams.spec.whatwg.org/) and draws inspiration from the browser Streams API while adapting to Rust's ownership model and async ecosystem.
