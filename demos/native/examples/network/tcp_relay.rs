//! Wrapping TCP sockets as a stream source and a stream sink.
//!
//! One socket is exposed as a byte `ReadableStream`, read through a BYOB reader that
//! fills a caller buffer; a second socket is exposed as a `WritableStream` sink. A
//! message written to the sink is echoed by the server and read back through the source
//! — the two halves a socket-to-socket relay is built from.
//!
//! Requires network access (connects to tcpbin.com:4242, a public echo server).
//!
//! Run with: `cargo run --example tcp_relay`

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use whatwg_streams::{
    ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult, WritableSink,
    WritableStream, WritableStreamDefaultController,
};

/// Byte source that connects to `addr` on start and reads from the socket.
pub struct TcpSource {
    socket: Option<TcpStream>,
    addr: String,
}

impl TcpSource {
    pub fn new(addr: &str) -> Self {
        Self {
            socket: None,
            addr: addr.to_string(),
        }
    }
}

impl ReadableByteSource for TcpSource {
    async fn start(&mut self, _controller: &mut ReadableByteStreamController) -> StreamResult<()> {
        self.socket = Some(TcpStream::connect(&self.addr).await?);
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        let socket = self.socket.as_mut().expect("connected in start");
        let n = socket.read(buffer).await?;
        if n == 0 {
            controller.close()?;
        }
        Ok(n)
    }

    async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
        self.socket = None;
        Ok(())
    }
}

/// Sink that writes each chunk to a TCP socket.
pub struct TcpSink {
    socket: TcpStream,
}

impl WritableSink<Vec<u8>> for TcpSink {
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.socket.write_all(&chunk).await?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        self.socket.shutdown().await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let addr = "tcpbin.com:4242"; // public echo server

    let source = ReadableStream::builder_bytes(TcpSource::new(addr)).spawn(tokio::spawn);
    let (_rlock, reader) = source.get_byob_reader().expect("a fresh stream is unlocked");

    let sink_socket = TcpStream::connect(addr).await.expect("connect sink socket");
    let sink = WritableStream::builder(TcpSink {
        socket: sink_socket,
    })
    .spawn(tokio::spawn);
    let (_wlock, writer) = sink.get_writer().expect("a fresh stream is unlocked");

    let message = b"hello relay!".to_vec();
    println!("sending: {}", String::from_utf8_lossy(&message));
    writer.write(message).await.expect("write to sink");

    let mut buf = vec![0u8; 64];
    let n = reader.read(&mut buf).await.expect("read from source");
    println!("received: {}", String::from_utf8_lossy(&buf[..n]));

    writer.close().await.expect("close sink");
}
