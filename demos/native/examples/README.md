# WHATWG Streams Examples

This directory contains comprehensive examples demonstrating real-world usage of the WHATWG Streams implementation in Rust.

## 📁 Directory Structure

### [`basic/`](basic/)

Fundamental stream operations - start here!

- **[`simple_readable.rs`](basic/simple_readable.rs)** - Basic readable stream creation and consumption
- **`simple_writable.rs`** - Basic writable stream operations
- **`simple_transform.rs`** - Simple 1:1 transforms

### [`file_processing/`](file_processing/)

Real-world file processing pipelines

- **[`log_filter.rs`](file_processing/log_filter.rs)** - Filter log files for ERROR entries
- **`csv_processor.rs`** - Stream CSV parsing and filtering
- **`json_stream.rs`** - Streaming JSON processing

### [`compression/`](compression/)

Data compression and decompression streams

- **[`gzip_transform.rs`](compression/gzip_transform.rs)** - Stream gzip compression
- **`decompress_stream.rs`** - Stream decompression
- **`archive_creator.rs`** - Multi-file archive creation

### [`bytes/`](bytes/)

Binary data and BYOB (Bring Your Own Buffer) examples

- **[`file_reader_byob.rs`](bytes/file_reader_byob.rs)** - Efficient large file reading with buffer reuse
- **`buffer_pool.rs`** - Buffer pooling patterns
- **`binary_protocol.rs`** - Binary protocol parsing

### [`network/`](network/)

Network streaming examples

- **`http_response.rs`** - Stream HTTP responses
- **`websocket_proxy.rs`** - WebSocket message transformation
- **`tcp_relay.rs`** - TCP data relay with transforms

### [`advanced/`](advanced/)

Complex scenarios and advanced patterns

- **[`multi_stage_pipeline.rs`](advanced/multi_stage_pipeline.rs)** - Complex pipeline with multiple transforms
- **`backpressure_demo.rs`** - Demonstrating backpressure handling
- **`error_recovery.rs`** - Error handling and recovery patterns

## 🚀 Quick Start

### Running Examples

Each example is self-contained and can be run with:

```bash
# Basic examples
cargo run --bin simple_readable

# File processing
cargo run --bin log_filter
cargo run --bin gzip_transform

# BYOB demonstrations
cargo run --bin file_reader_byob

# Complex pipelines
cargo run --bin multi_stage_pipeline
```

### Key Learning Path

1. **Start with basics**: `simple_readable.rs` → understand stream fundamentals
2. **Try file processing**: `log_filter.rs` → see real-world transforms
3. **Explore compression**: `gzip_transform.rs` → understand stateful transforms
4. **Learn BYOB**: `file_reader_byob.rs` → when and how to use byte streams
5. **Complex pipelines**: `multi_stage_pipeline.rs` → chain multiple transforms

## 🎯 Key Concepts Demonstrated

### Stream Types

- **ReadableStream<T>** - For structured data (strings, objects, etc.)
- **ReadableByteStream** - For binary data with BYOB support
- **WritableStream<T>** - For outputting data
- **TransformStream<I, O>** - For transforming data from type I to type O

### When to Use What

- **Default Streams** - Most use cases (data processing, transforms, piping)
- **BYOB Streams** - Large file reading, network streams, performance-critical scenarios
- **Transform Streams** - Any data transformation (filtering, parsing, compression)

### Pipeline Patterns

```rust
// Simple pipeline
readable_stream
    .pipe_through(transform_stream, None)
    .pipe_to(&writable_stream, None)
    .await?;

// Multi-stage pipeline
source
    .pipe_through(parse_transform, None)
    .pipe_through(filter_transform, None)
    .pipe_through(compress_transform, None)
    .pipe_to(&output_stream, None)
    .await?;
```

## 📊 Performance Considerations

### BYOB Benefits

✅ **Use BYOB when**:

- Reading large files (>1MB)
- Network streams with high throughput
- Need to control buffer allocation
- Measurable allocation overhead

❌ **Don't use BYOB when**:

- Small data processing
- Using pipe operations (they use default readers)
- Transform operations (work with data, not buffers)

### Transform Performance

- Transforms work with data chunks, not individual bytes
- Even binary transforms (compression) use default streams
- Pipeline throughput is limited by slowest stage
- Use appropriate buffer sizes in queuing strategies

## 🔧 Implementation Notes

### Error Handling

All examples demonstrate proper error handling:

- Source errors propagate through pipelines
- Transform errors stop the pipeline
- Resources are cleaned up properly

### Resource Management

- Files are properly closed
- Async operations use appropriate spawning
- Memory usage is controlled via queuing strategies

### Testing

Each example includes comprehensive tests showing:

- Normal operation
- Error conditions
- Edge cases (empty data, large data)
- Performance characteristics

## 🎓 Learning Resources

### Understanding Backpressure

Backpressure automatically handles cases where:

- Producer is faster than consumer
- Consumer is faster than producer
- Multiple stages have different processing rates

### Stream Lifecycle

1. **Start** - Initialize resources
2. **Pull/Write** - Process data chunks
3. **Close/Flush** - Clean up and finalize
4. **Error** - Handle failures gracefully

### Best Practices

- Use appropriate chunk sizes (not too small, not too large)
- Handle errors at appropriate levels
- Choose correct stream types for your use case
- Test with realistic data sizes

## 🐛 Common Pitfalls

1. **Using BYOB for transforms** - Transforms should use default streams
2. **Not handling errors** - Always handle stream errors appropriately
3. **Wrong buffer sizes** - Too small = overhead, too large = memory waste
4. **Forgetting to close** - Always close streams to free resources
5. **Blocking operations** - Use async operations to avoid blocking the runtime

---

These examples demonstrate the power and flexibility of WHATWG Streams in Rust. Start with the basics and work your way up to complex pipelines!
