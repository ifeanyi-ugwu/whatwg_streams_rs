# whatwg_streams examples

A gallery of standalone programs exercising the [`whatwg_streams`](../../whatwg_streams_rs)
crate, a Rust port of the WHATWG Streams API. Each file is self-contained; run one with:

```bash
cargo run --example <name>
```

## Examples

### basic/

- `simple_readable` — implement a `ReadableSource` and read from the stream
- `simple_writable` — implement a `WritableSink` and write to it
- `simple_transform` — pipe a source through a transform into a sink

### bytes/

- `binary_protocol` — parse length-prefixed frames split across chunk boundaries
- `file_reader_byob` — read a file with a BYOB reader and caller-chosen buffer sizes
- `buffer_pool` — reuse buffers across BYOB reads with a small pool

### compression/

- `gzip_transform` — streaming gzip compression, confirmed by a round-trip
- `decompress_stream` — streaming gzip decompression (pairs with `gzip_transform`)
- `archive_creator` — stream files into a `.tar.gz`, one tar block at a time

### file_processing/

- `log_filter` — filter log lines to ERROR and write them to a file
- `csv_processor` — filter CSV rows by a column value
- `json_stream` — stream records out of an NDJSON file, parsing each line as it arrives

### advanced/

- `backpressure_demo` — watch a fast producer stall against a slow sink (prints `desired_size`)
- `multi_stage_pipeline` — a five-stage pipeline that changes type at each step, output as gzipped NDJSON

### network/

- `http_response` — stream an HTTP response body off a `TcpStream` *(requires network)*
- `tcp_relay` — wrap TCP sockets as a stream source and sink *(requires network)*
- `websocket_proxy` — a self-contained bidirectional WebSocket proxy over byte streams

## The pipeline pattern

Build each piece with `::builder(..).spawn(spawn_fn)`, then compose:

```rust
let source = ReadableStream::builder(MySource).spawn(tokio::spawn);
let transform = TransformStream::builder(MyTransform).spawn(tokio::spawn);
let sink = WritableStream::builder(MySink).spawn(tokio::spawn);

// pipe_through hands back the transform's readable side; pipe_to drives it to the sink.
let out = source.pipe_through(transform, None).spawn(tokio::spawn);
out.pipe_to(&sink, None).await?;
```

For byte streams, build with `builder_bytes(..)` and read via `get_byob_reader()`.

## Notes
- `http_response` and `tcp_relay` reach the public internet; the rest run offline.
- `gzip_transform` writes `compressed_data.gz`, which `decompress_stream` reads back —
  run them in that order.
