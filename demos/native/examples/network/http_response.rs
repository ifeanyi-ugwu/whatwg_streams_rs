//! Streaming an HTTP response body through a `ReadableStream`.
//!
//! A raw `TcpStream` is wrapped as a byte-producing source: each `pull` reads up to
//! `buffer_size` bytes off the socket and enqueues them, closing when the server hangs up
//! (the request asks for `Connection: close`). The reader consumes the response in chunks
//! as they arrive off the wire, rather than buffering the whole body first.
//!
//! Requires network access (connects to example.com:80).
//!
//! Run with: `cargo run --example http_response`

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult,
};

/// Reads an HTTP response body off a TCP socket, one buffer at a time.
pub struct HttpResponseSource {
    stream: TcpStream,
    buffer_size: usize,
}

impl HttpResponseSource {
    pub fn new(stream: TcpStream, buffer_size: usize) -> Self {
        Self {
            stream,
            buffer_size,
        }
    }
}

impl ReadableSource<Vec<u8>> for HttpResponseSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let mut buf = vec![0u8; self.buffer_size];
        let n = self.stream.read(&mut buf).await?;
        if n == 0 {
            controller.close()?;
        } else {
            buf.truncate(n);
            controller.enqueue(buf)?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let mut stream = TcpStream::connect("example.com:80")
        .await
        .expect("connect to example.com:80");
    let request = b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
    stream.write_all(request).await.expect("send request");

    let response =
        ReadableStream::builder(HttpResponseSource::new(stream, 1024)).spawn(tokio::spawn);
    let (_lock, reader) = response.get_reader().expect("a fresh stream is unlocked");

    let mut total = 0;
    while let Some(chunk) = reader.read().await.expect("read should not error") {
        total += chunk.len();
        println!("received {} bytes", chunk.len());
    }
    println!("response complete: {total} bytes total");
}
