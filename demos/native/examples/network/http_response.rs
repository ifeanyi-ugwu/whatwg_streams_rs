//! HTTP Response Stream Example
//!
//! Demonstrates reading from an HTTP server and streaming the response body
//! using the same pipeline pattern as other examples.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
};

/// A simple readable source that reads from a TCP stream (HTTP response)
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
            controller.enqueue(buf[..n].to_vec())?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🌐 HTTP Response Stream Example");

    // Connect to HTTP server
    let mut stream = TcpStream::connect("example.com:80").await?;
    let request = b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
    stream.write_all(request).await?;

    // Wrap TCP stream in our readable source
    let readable = ReadableStream::new_default(HttpResponseSource::new(stream, 1024), None);

    // Read and print chunks as they arrive
    let mut reader = readable.get_reader().1;
    while let Some(chunk) = reader.read().await? {
        println!("📦 Received {} bytes", chunk.len());
        // Could process chunks here (parse headers, decompress, etc.)
    }

    println!("✅ HTTP response fully received");
    Ok(())
}
