//! TCP Relay Example
//!
//! Demonstrates how to use WHATWG-like streams to relay data between
//! two TCP sockets with optional transformations. This is the basic
//! "proxy pattern": client <-> relay <-> server.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableByteSource, ReadableByteStreamController, ReadableStream},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Readable source wrapping a TCP socket
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
        println!("🔌 Connecting to {}", self.addr);
        let stream = TcpStream::connect(&self.addr).await?;
        self.socket = Some(stream);
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        if let Some(socket) = self.socket.as_mut() {
            match socket.read(buffer).await {
                Ok(0) => {
                    println!("⛔ TCP closed by remote");
                    controller.close()?;
                    Ok(0)
                }
                Ok(n) => {
                    println!("📥 Read {} byte(s) from {}", n, self.addr);
                    Ok(n)
                }
                Err(e) => Err(e.into()),
            }
        } else {
            Ok(0)
        }
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        println!("🚫 TCP source cancelled: {:?}", reason);
        self.socket = None;
        Ok(())
    }
}

/// Writable sink wrapping a TCP socket
pub struct TcpSink {
    socket: TcpStream,
}

impl WritableSink<Vec<u8>> for TcpSink {
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        println!("📤 Forwarding {} byte(s)", chunk.len());
        self.socket.write_all(&chunk).await?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        println!("✅ Closing sink socket");
        self.socket.shutdown().await?;
        Ok(())
    }
}

/// Example: relay data from one TCP connection to another with a transform.
pub async fn run_tcp_relay_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== TCP Relay Example ===");

    // connect to server (echo)
    let server_addr = "tcpbin.com:4242"; // public test echo server
    let client_source = ReadableStream::builder(TcpSource::new(server_addr))
        .with_spawn(|fut| {
            tokio::spawn(fut);
        })
        .build();
    let reader = client_source.get_byob_reader().1;

    // second connection acts as sink (another echo)
    let sink_stream = TcpStream::connect(server_addr).await?;
    let sink = TcpSink {
        socket: sink_stream,
    };
    let writable =
        WritableStream::new_with_spawn(sink, Box::new(CountQueuingStrategy::new(1)), |fut| {
            tokio::spawn(fut);
        });
    let (_, writer) = writable.get_writer()?;

    // Send a message through the relay
    let msg = b"hello relay!".to_vec();
    println!("➡️ sending: {:?}", String::from_utf8_lossy(&msg));
    writer.write(msg).await?;

    // Read back the echo
    let mut buf = vec![0u8; 64];
    let n = reader.read(&mut buf).await?;
    if n > 0 {
        let received = &buf[..n];
        println!("✅ received: {:?}", String::from_utf8_lossy(received));
    }

    writer.close().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_tcp_relay_example().await
}
