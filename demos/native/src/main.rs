//! Index for the example gallery. The examples are the point — this binary just lists
//! them. Run one with: `cargo run --example <name>`.

fn main() {
    let gallery: &[(&str, &[(&str, &str)])] = &[
        (
            "basic",
            &[
                ("simple_readable", "implement a ReadableSource and read from the stream"),
                ("simple_writable", "implement a WritableSink and write to it"),
                ("simple_transform", "pipe a source through a transform into a sink"),
            ],
        ),
        (
            "bytes",
            &[
                ("binary_protocol", "parse length-prefixed frames split across chunks"),
                ("file_reader_byob", "BYOB reads with caller-chosen buffer sizes"),
                ("buffer_pool", "reuse buffers across BYOB reads with a pool"),
            ],
        ),
        (
            "compression",
            &[
                ("gzip_transform", "streaming gzip compression with a round-trip check"),
                ("decompress_stream", "streaming gzip decompression (pairs with gzip_transform)"),
                ("archive_creator", "stream files into a .tar.gz incrementally"),
            ],
        ),
        (
            "file_processing",
            &[
                ("log_filter", "filter log lines to ERROR, write to a file"),
                ("csv_processor", "filter CSV rows by a column value"),
                ("json_stream", "stream records from an NDJSON file"),
            ],
        ),
        (
            "advanced",
            &[
                ("backpressure_demo", "watch a fast producer stall against a slow sink"),
                ("multi_stage_pipeline", "five-stage pipeline, type changes each step"),
            ],
        ),
        (
            "network",
            &[
                ("http_response", "stream an HTTP response body (needs network)"),
                ("tcp_relay", "wrap TCP sockets as a source and sink (needs network)"),
                ("websocket_proxy", "self-contained bidirectional WebSocket proxy"),
            ],
        ),
    ];

    println!("whatwg_streams examples — run one with: cargo run --example <name>\n");
    for (category, items) in gallery {
        println!("{category}:");
        for (name, desc) in *items {
            println!("  {name:<22}{desc}");
        }
        println!();
    }
}
