//! WebSocket BYOB Proxy Example
//!
//! This program runs a WebSocket proxy (listening on 127.0.0.1:8080) that forwards binary
//! frames to ws://echo.websocket.events and back, using BYOB-style readable streams.
//!
//! It also includes a tiny built-in test client that connects to the proxy, sends a binary
//! payload, reads the proxied echo, prints the result, and exits.
//!
//! Run: `cargo run`
//!
//! Note: The demo uses a short sleep to allow the proxy to bind before the built-in client attempts to connect.

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::fmt::Display;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::sleep;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::protocol::Message};
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy,
    readable_sampling_b::{ReadableByteSource, ReadableByteStreamController, ReadableStream},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// A BYOB source that reads binary frames from a WebSocket *stream half* (the `Stream`).
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
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin
        + Send
        + 'static,
{
    async fn start(
        &mut self,
        _controller: &mut ReadableByteStreamController,
    ) -> whatwg_streams::dlc::ideation::d::StreamResult<()> {
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> whatwg_streams::dlc::ideation::d::StreamResult<usize> {
        if self.current_frame.is_none() {
            match self.ws_stream.next().await {
                Some(Ok(Message::Binary(data))) => {
                    self.current_frame = Some(data.into());
                }
                Some(Ok(Message::Close(_))) | None => {
                    controller
                        .close()
                        .map_err(|e| format!("Failed to close controller: {}", e))?;
                    return Ok(0);
                }
                Some(Ok(Message::Text(_))) => {
                    // ignoring text frames for this binary example
                    return Ok(0);
                }
                Some(Ok(_)) => {
                    // ping/pong - ignore
                    return Ok(0);
                }
                /*Some(Err(e)) => {
                    return Err(format!("WebSocket read error: {}", e).into());
                }*/
                Some(Err(e)) => {
                    let msg = e.to_string();
                    if msg.contains("Connection reset without closing handshake") {
                        controller
                            .close()
                            .map_err(|e| format!("Failed to close controller: {}", e))?;
                        return Ok(0);
                    } else {
                        return Err(format!("WebSocket read error: {}", e).into());
                    }
                }
            }
        }

        if let Some(frame) = self.current_frame.as_mut() {
            let n = frame.len().min(buffer.len());
            buffer[..n].copy_from_slice(&frame[..n]);
            frame.drain(..n);
            if frame.is_empty() {
                self.current_frame = None;
            }
            Ok(n)
        } else {
            Ok(0)
        }
    }

    async fn cancel(
        &mut self,
        _reason: Option<String>,
    ) -> whatwg_streams::dlc::ideation::d::StreamResult<()> {
        // Nothing to do for stream half here
        Ok(())
    }
}

/// A writable sink that sends binary frames to a WebSocket *sink half* (the `Sink`).
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
    S: Sink<Message> + Unpin + Send,
    <S as Sink<Message>>::Error: Display,
{
    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _controller: &mut WritableStreamDefaultController,
    ) -> whatwg_streams::dlc::ideation::d::StreamResult<()> {
        self.ws_sink
            .send(Message::Binary(chunk.into()))
            .await
            .map_err(|e| format!("WebSocket send error: {}", e))?;
        Ok(())
    }

    async fn close(mut self) -> whatwg_streams::dlc::ideation::d::StreamResult<()> {
        let _ = self.ws_sink.send(Message::Close(None)).await;
        Ok(())
    }
}

/// Start the proxy loop (non-blocking). Each client accepted spawns two forwarding tasks.
pub async fn start_proxy() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("🚀 Proxy listening on ws://127.0.0.1:8080");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp_stream, _peer)) => {
                    tokio::spawn(async move {
                        match accept_async(tcp_stream).await {
                            Ok(ws_client) => {
                                println!("🌐 Client connected to proxy");

                                // Connect to upstream server
                                let (ws_upstream, _resp) =
                                    match connect_async("ws://127.0.0.1:9001").await {
                                        Ok(v) => v,
                                        Err(e) => {
                                            eprintln!("🔌 Upstream connect failed: {}", e);
                                            return;
                                        }
                                    };
                                println!("🔗 Proxy connected to upstream server");

                                // Split client and upstream into sink + stream halves
                                let (client_ws_sink, client_ws_stream) = ws_client.split();
                                let (upstream_ws_sink, upstream_ws_stream) = ws_upstream.split();

                                // client -> upstream: read from client stream (BYOB), write to upstream sink
                                let client_reader_stream = ReadableStream::builder(
                                    WebSocketByobSource::new(client_ws_stream),
                                )
                                .build();
                                let (_, client_byob_reader) =
                                    client_reader_stream.get_byob_reader();

                                let upstream_writable = WritableStream::new_with_spawn(
                                    WebSocketSink::new(upstream_ws_sink),
                                    Box::new(CountQueuingStrategy::new(3)),
                                    |fut| {
                                        tokio::spawn(fut);
                                    },
                                );
                                let (_, upstream_writer) = match upstream_writable.get_writer() {
                                    Ok(x) => x,
                                    Err(e) => {
                                        eprintln!(
                                            "Failed to get writer for upstream sink: {:?}",
                                            e
                                        );
                                        return;
                                    }
                                };

                                // upstream -> client: read from upstream stream (BYOB), write to client sink
                                let upstream_reader_stream = ReadableStream::builder(
                                    WebSocketByobSource::new(upstream_ws_stream),
                                )
                                .build();
                                let (_, upstream_byob_reader) =
                                    upstream_reader_stream.get_byob_reader();

                                let client_writable = WritableStream::new_with_spawn(
                                    WebSocketSink::new(client_ws_sink),
                                    Box::new(CountQueuingStrategy::new(3)),
                                    |fut| {
                                        tokio::spawn(fut);
                                    },
                                );
                                let (_, client_writer) = match client_writable.get_writer() {
                                    Ok(x) => x,
                                    Err(e) => {
                                        eprintln!("Failed to get writer for client sink: {:?}", e);
                                        return;
                                    }
                                };

                                // Forward client -> upstream
                                let forward_c2u = async move {
                                    let mut buf = vec![0u8; 4096];
                                    loop {
                                        match client_byob_reader.read(&mut buf).await {
                                            Ok(0) => {
                                                let _ = upstream_writer.close().await;
                                                break;
                                            }
                                            Ok(n) => {
                                                let data = buf[..n].to_vec();
                                                if let Err(e) = upstream_writer.write(data).await {
                                                    eprintln!("Error writing to upstream: {:?}", e);
                                                    let _ = upstream_writer.close().await;
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "Error reading from client BYOB: {:?}",
                                                    e
                                                );
                                                let _ = upstream_writer.close().await;
                                                break;
                                            }
                                        }
                                    }
                                };

                                // Forward upstream -> client
                                let forward_u2c = async move {
                                    let mut buf = vec![0u8; 4096];
                                    loop {
                                        match upstream_byob_reader.read(&mut buf).await {
                                            Ok(0) => {
                                                let _ = client_writer.close().await;
                                                break;
                                            }
                                            Ok(n) => {
                                                let data = buf[..n].to_vec();
                                                if let Err(e) = client_writer.write(data).await {
                                                    eprintln!("Error writing to client: {:?}", e);
                                                    let _ = client_writer.close().await;
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                eprintln!(
                                                    "Error reading from upstream BYOB: {:?}",
                                                    e
                                                );
                                                let _ = client_writer.close().await;
                                                break;
                                            }
                                        }
                                    }
                                };

                                tokio::spawn(forward_c2u);
                                tokio::spawn(forward_u2c);
                            }
                            Err(e) => {
                                eprintln!("Failed to accept client websocket: {}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    eprintln!("Listener accept error: {}", e);
                    // small pause to avoid busy-loop on hard errors
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    Ok(())
}

/// Simple local echo WebSocket server for testing.
pub async fn start_echo_server() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:9001").await?;
    println!("🔁 Echo server listening on ws://127.0.0.1:9001");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp_stream, _)) => {
                    tokio::spawn(async move {
                        if let Ok(ws_stream) = accept_async(tcp_stream).await {
                            let (mut sink, mut stream) = ws_stream.split();
                            while let Some(msg) = stream.next().await {
                                if let Ok(msg) = msg {
                                    // Echo back binary/text messages
                                    if msg.is_binary() || msg.is_text() {
                                        if let Err(e) = sink.send(msg).await {
                                            eprintln!("Echo send error: {}", e);
                                            break;
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(e) => eprintln!("Echo accept error: {}", e),
            }
        }
    });

    Ok(())
}

/// Simple test client that connects to the proxy, sends a binary payload, and waits for an echo.
async fn run_test_client_via_proxy() -> Result<(), Box<dyn std::error::Error>> {
    // Give the proxy a moment to start
    sleep(Duration::from_millis(200)).await;

    let url = "ws://127.0.0.1:8080";
    println!("🔎 Test client connecting to proxy at {}", url);

    let (mut ws_stream, _resp) = connect_async(url).await?;
    println!("✅ Test client connected to proxy");

    // send a binary payload (e.g., "Hello")
    let test_payload: Vec<u8> = b"Hello from client through proxy".to_vec();
    ws_stream
        .send(Message::Binary(test_payload.clone().into()))
        .await?;
    println!("📤 Test client sent {} bytes", test_payload.len());

    // Wait for a binary reply (proxied echo)
    while let Some(msg) = ws_stream.next().await {
        match msg {
            Ok(Message::Binary(data)) => {
                println!(
                    "📥 Test client received {} bytes via proxy: {:?}",
                    data.len(),
                    &data
                );
                if data == test_payload {
                    println!("🎉 Echo match! Proxy forwarded correctly.");
                } else {
                    println!("⚠️ Reply did not match original payload (upstream may modify).");
                }
                break;
            }
            Ok(Message::Text(txt)) => {
                println!("📥 Test client received text via proxy: {}", txt);
                // continue waiting for binary
            }
            Ok(Message::Close(_)) => {
                println!("🔌 Test client received close");
                break;
            }
            Ok(_) => {
                // ping/pong etc
            }
            Err(e) => {
                eprintln!("Test client websocket error: {}", e);
                break;
            }
        }
    }

    // close test client connection
    let _ = ws_stream.close(None).await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Start local echo server
    start_echo_server().await;

    // Start proxy (non-blocking)
    start_proxy().await;

    // Run the test client that connects to the proxy and demonstrates a round-trip.
    if let Err(e) = run_test_client_via_proxy().await {
        eprintln!("Test client failed: {}", e);
    }

    // Small pause to let background proxy tasks print any remaining logs
    sleep(Duration::from_millis(200)).await;

    println!("✅ Demo finished — exiting.");
    Ok(())
}
