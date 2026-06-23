// WritableStream implements futures::Sink<T>.
// These tests exercise the Sink contract: poll_ready / start_send / poll_flush / poll_close.
// WPT has no direct equivalent (it is JS-specific); this covers the Rust trait surface.

use crate::helpers::{CollectSink, LifecycleSink};
use whatwg_streams::{CountQueuingStrategy, WritableStream};

// ── SinkExt::send ─────────────────────────────────────────────────────────────

// "Sink: send() delivers a chunk to the underlying sink"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_send_delivers_chunk() {
    use futures::SinkExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    stream.send(1u32).await.unwrap();
    stream.send(2u32).await.unwrap();
    stream.send(3u32).await.unwrap();

    // flush + close so writes land before we inspect
    stream.close().await.unwrap();
    assert_eq!(*collected.lock().unwrap(), vec![1, 2, 3]);
}

// "Sink: send_all() forwards every item from an iterator"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_send_all_delivers_all_chunks() {
    use futures::{SinkExt, StreamExt, stream};

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    let items = vec![10u32, 20, 30];
    let mut src = stream::iter(items.iter().cloned().map(Ok::<u32, whatwg_streams::StreamError>));
    stream.send_all(&mut src).await.unwrap();

    stream.close().await.unwrap();
    assert_eq!(*collected.lock().unwrap(), vec![10, 20, 30]);
}

// ── poll_flush ────────────────────────────────────────────────────────────────

// "Sink: poll_flush waits for all enqueued writes before resolving"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_flush_waits_for_writes() {
    use futures::SinkExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    // start_send without awaiting — fire-and-forget via the Sink API
    futures::Sink::start_send(std::pin::Pin::new(&mut stream), 7u32).unwrap();
    futures::Sink::start_send(std::pin::Pin::new(&mut stream), 8u32).unwrap();

    stream.flush().await.unwrap();
    assert_eq!(*collected.lock().unwrap(), vec![7, 8]);
}

// ── poll_close ────────────────────────────────────────────────────────────────

// "Sink: poll_close calls sink.close() and transitions stream to closed"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_close_calls_sink_close() {
    use futures::SinkExt;

    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = LifecycleSink {
        closed: closed.clone(),
        ..Default::default()
    };
    let mut stream = WritableStream::builder(sink).spawn(tokio::spawn);

    stream.close().await.unwrap();
    assert!(*closed.lock().unwrap(), "sink.close() should have been called");
}

// "Sink: poll_close on an already-closed stream returns Ok"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_close_on_already_closed_is_ok() {
    use futures::SinkExt;

    let mut stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    stream.close().await.unwrap();
    // Per spec (WPT close.any.js test 24): second close rejects — does not panic or hang
    assert!(stream.close().await.is_err(), "second close() must reject per spec");
}

// ── poll_ready ────────────────────────────────────────────────────────────────

// "Sink: poll_ready resolves immediately when there is no backpressure"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_poll_ready_resolves_when_no_backpressure() {
    use futures::Sink;
    use std::pin::Pin;

    let mut stream = WritableStream::builder(LifecycleSink::default())
        .strategy(CountQueuingStrategy::new(4))
        .spawn(tokio::spawn);

    // Drive poll_ready as a future via poll_fn
    let result = futures::future::poll_fn(|cx| {
        Pin::new(&mut stream).poll_ready(cx)
    })
    .await;

    assert!(result.is_ok(), "poll_ready should resolve Ok when queue is empty");
}

// "Sink: send() after close() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn sink_send_after_close_errors() {
    use futures::SinkExt;

    let mut stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    stream.close().await.unwrap();
    assert!(
        stream.send(1u32).await.is_err(),
        "send() after close() must return an error"
    );
}
