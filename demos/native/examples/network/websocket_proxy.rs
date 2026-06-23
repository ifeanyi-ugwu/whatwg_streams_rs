//! A bidirectional WebSocket proxy built from byte streams.
//!
//! Everything runs locally, so the demo is self-contained:
//!   - an echo server on ws://127.0.0.1:9001 (the upstream),
//!   - a proxy on ws://127.0.0.1:8080 that forwards binary frames client <-> upstream,
//!   - a built-in test client that connects to the proxy, sends a payload, and reads the
//!     echoed reply back.
//!
//! Each WebSocket half is wrapped in the streams API: the receive half becomes a byte
//! `ReadableStream` (read via a BYOB reader), the send half a `WritableStream` sink. Each
//! accepted client spawns two forwarding tasks, one per direction.
//!
//! Run with: `cargo run --example websocket_proxy`

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::fmt::Display;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::sleep;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::protocol::Message};
use whatwg_streams::{
    ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult, WritableSink,
    WritableStream, WritableStreamDefaultController,
};

/// BYOB source over the receive half of a WebSocket, yielding binary frame bytes.
pub struct WebSocketByobSource<S> {
    ws_stream: S,
    current_frame: Option<Vec<u8>>,
}

impl<S> WebSocketByobSource<S> {
    pub fn new(ws_stream: S) -> Self {
        Self {
            ws_stream,
            current_frame: None,
        }
    }
}

impl<S> ReadableByteSource for WebSocketByobSource<S>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin + Send + 'static,
{
    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        if self.current_frame.is_none() {
            match self.ws_stream.next().await {
                Some(Ok(Message::Binary(data))) => self.current_frame = Some(data.into()),
                Some(Ok(Message::Close(_))) | None => {
                    controller.close()?;
                    return Ok(0);
                }
                // A peer hanging up without a close handshake is a normal end-of-stream here.
                Some(Err(e)) if e.to_string().contains("Connection reset without closing") => {
                    controller.close()?;
                    return Ok(0);
                }
                Some(Err(e)) => return Err(format!("WebSocket read error: {e}").into()),
                Some(Ok(_)) => return Ok(0), // text / ping / pong — ignored in this binary proxy
            }
        }

        let frame = self.current_frame.as_mut().expect("frame set above");
        let n = frame.len().min(buffer.len());
        buffer[..n].copy_from_slice(&frame[..n]);
        frame.drain(..n);
        if frame.is_empty() {
            self.current_frame = None;
        }
        Ok(n)
    }
}

/// Sink over the send half of a WebSocket, writing each chunk as a binary frame.
pub struct WebSocketSink<S> {
    ws_sink: S,
}

impl<S> WebSocketSink<S> {
    pub fn new(ws_sink: S) -> Self {
        Self { ws_sink }
    }
}

impl<S> WritableSink<Vec<u8>> for WebSocketSink<S>
where
    S: Sink<Message> + Unpin + Send + 'static,
    <S as Sink<Message>>::Error: Display,
{
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.ws_sink
            .send(Message::Binary(chunk.into()))
            .await
            .map_err(|e| format!("WebSocket send error: {e}"))?;
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        let _ = self.ws_sink.send(Message::Close(None)).await;
        Ok(())
    }
}

/// Accepts proxy clients; each spawns two forwarding tasks (client->upstream, upstream->client).
pub async fn start_proxy() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("proxy listening on ws://127.0.0.1:8080");

    tokio::spawn(async move {
        while let Ok((tcp, _peer)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(client_ws) = accept_async(tcp).await else {
                    return;
                };
                let Ok((upstream_ws, _)) = connect_async("ws://127.0.0.1:9001").await else {
                    eprintln!("proxy: upstream connect failed");
                    return;
                };

                let (client_sink, client_stream) = client_ws.split();
                let (upstream_sink, upstream_stream) = upstream_ws.split();

                // client -> upstream
                let c2u_read = ReadableStream::builder_bytes(WebSocketByobSource::new(client_stream))
                    .spawn(tokio::spawn);
                let (_, client_reader) = c2u_read.get_byob_reader().expect("unlocked");
                let u_write = WritableStream::builder(WebSocketSink::new(upstream_sink)).spawn(tokio::spawn);
                let (_, upstream_writer) = u_write.get_writer().expect("unlocked");

                // upstream -> client
                let u2c_read = ReadableStream::builder_bytes(WebSocketByobSource::new(upstream_stream))
                    .spawn(tokio::spawn);
                let (_, upstream_reader) = u2c_read.get_byob_reader().expect("unlocked");
                let c_write = WritableStream::builder(WebSocketSink::new(client_sink)).spawn(tokio::spawn);
                let (_, client_writer) = c_write.get_writer().expect("unlocked");

                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = client_reader.read(&mut buf).await {
                        if n == 0 || upstream_writer.write(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    let _ = upstream_writer.close().await;
                });
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    while let Ok(n) = upstream_reader.read(&mut buf).await {
                        if n == 0 || client_writer.write(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                    let _ = client_writer.close().await;
                });
            });
        }
    });

    Ok(())
}

/// Local echo server: sends every binary/text frame straight back.
pub async fn start_echo_server() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:9001").await?;
    println!("echo server listening on ws://127.0.0.1:9001");

    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(ws) = accept_async(tcp).await else {
                    return;
                };
                let (mut sink, mut stream) = ws.split();
                while let Some(Ok(msg)) = stream.next().await {
                    if (msg.is_binary() || msg.is_text()) && sink.send(msg).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    Ok(())
}

/// Connects to the proxy, sends a binary payload, and waits for the echoed reply.
async fn run_test_client() -> Result<(), Box<dyn std::error::Error>> {
    sleep(Duration::from_millis(200)).await; // let the listeners bind

    let (mut ws, _) = connect_async("ws://127.0.0.1:8080").await?;
    println!("client connected to proxy");

    let payload = b"Hello from client through proxy".to_vec();
    ws.send(Message::Binary(payload.clone().into())).await?;
    println!("client sent {} bytes", payload.len());

    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Binary(data) => {
                let matched = data == payload;
                println!("client received {} bytes (echo match: {matched})", data.len());
                break;
            }
            Message::Close(_) => break,
            _ => {} // keep waiting for the binary echo
        }
    }

    let _ = ws.close(None).await;
    Ok(())
}

#[tokio::main]
async fn main() {
    start_echo_server().await.expect("start echo server");
    start_proxy().await.expect("start proxy");

    if let Err(e) = run_test_client().await {
        eprintln!("test client failed: {e}");
    }

    sleep(Duration::from_millis(200)).await; // let background tasks drain
    println!("done.");
}
