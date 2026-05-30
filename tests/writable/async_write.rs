// WritableStream implements futures::AsyncWrite when T: for<'a> From<&'a [u8]>.
// These tests exercise the AsyncWrite contract: poll_write / poll_flush / poll_close.
// WPT has no direct equivalent; this covers the Rust trait surface.

use crate::helpers::CollectSink;
use whatwg_streams::{CountQueuingStrategy, WritableStream};

// ── AsyncWriteExt helpers ─────────────────────────────────────────────────────

// "AsyncWrite: write_all() delivers all bytes to the underlying sink as Vec<u8>"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_all_delivers_bytes() {
    use futures::AsyncWriteExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    // Vec<u8> implements From<&[u8]>, so WritableStream<Vec<u8>, _> satisfies AsyncWrite
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    stream.write_all(b"hello world").await.unwrap();
    stream.flush().await.unwrap();
    stream.close().await.unwrap();

    let written: Vec<u8> = collected.lock().unwrap().iter().flatten().cloned().collect();
    assert_eq!(written, b"hello world");
}

// "AsyncWrite: write() returns the number of bytes written"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_returns_byte_count() {
    use futures::AsyncWriteExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    let n = stream.write(b"test data").await.unwrap();
    assert_eq!(n, 9, "write() should report the number of bytes in the slice");
}

// "AsyncWrite: writing an empty slice is a no-op"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_empty_slice_is_noop() {
    use futures::AsyncWriteExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink).spawn(tokio::spawn);

    let n = stream.write(b"").await.unwrap();
    assert_eq!(n, 0);

    // Nothing should have been enqueued
    stream.flush().await.unwrap();
    assert!(collected.lock().unwrap().is_empty());
}

// "AsyncWrite: multiple writes arrive in order"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_multiple_writes_in_order() {
    use futures::AsyncWriteExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    stream.write_all(b"foo").await.unwrap();
    stream.write_all(b"bar").await.unwrap();
    stream.write_all(b"baz").await.unwrap();
    stream.flush().await.unwrap();
    stream.close().await.unwrap();

    let written: Vec<u8> = collected.lock().unwrap().iter().flatten().cloned().collect();
    assert_eq!(written, b"foobarbaz");
}

// "AsyncWrite: poll_flush waits for all prior writes to land"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_flush_waits_for_writes() {
    use futures::AsyncWriteExt;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    stream.write_all(b"abc").await.unwrap();
    stream.flush().await.unwrap();

    let written: Vec<u8> = collected.lock().unwrap().iter().flatten().cloned().collect();
    assert_eq!(written, b"abc", "flush() must ensure prior writes reached the sink");
}

// "AsyncWrite: poll_close closes the underlying stream"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_close_closes_stream() {
    use futures::AsyncWriteExt;

    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<Vec<u8>> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink).spawn(tokio::spawn);

    stream.close().await.unwrap();
    assert!(*closed.lock().unwrap(), "AsyncWrite::close() should call sink.close()");
}

// "AsyncWrite: write() after close() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_after_close_errors() {
    use futures::AsyncWriteExt;

    let sink = CollectSink::<Vec<u8>> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink).spawn(tokio::spawn);

    stream.close().await.unwrap();
    assert!(
        stream.write(b"too late").await.is_err(),
        "write() after close() must return an error"
    );
}

// "AsyncWrite: copy_buf() works end-to-end via the AsyncWrite + AsyncBufRead bridge"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_write_copy_buf_works() {
    use futures::{AsyncWriteExt, io::Cursor};

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let mut stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    let mut src = Cursor::new(b"copy source data");
    futures::io::copy_buf(&mut src, &mut stream).await.unwrap();
    stream.close().await.unwrap();

    let written: Vec<u8> = collected.lock().unwrap().iter().flatten().cloned().collect();
    assert_eq!(written, b"copy source data");
}
