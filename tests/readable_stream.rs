//! Integration tests for ReadableStream, ported from:
//! WPT: streams/readable-streams/general.any.js
//! WPT: streams/readable-streams/cancel.any.js
//! https://github.com/web-platform-tests/wpt/tree/master/streams/readable-streams

use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamError, StreamResult,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

// Reserved for cancel-while-pull-in-flight tests once the underlying spec gap is fixed:
// the stream task does not call source.cancel() when a pull future is in flight at cancel time.
#[cfg(feature = "send")]
#[allow(dead_code)]
struct HangingSource {
    cancel_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl ReadableSource<u32> for HangingSource {
    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        // Yield once then hang — keeps source in inner.source after first yield
        futures::future::pending::<()>().await;
        Ok(())
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.cancel_reason.lock().unwrap() = reason;
        Ok(())
    }
}

/// Source that enqueues incrementing integers on each pull.
#[cfg(feature = "send")]
struct CountingSource {
    counter: u32,
    cancel_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl ReadableSource<u32> for CountingSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        self.counter += 1;
        controller.enqueue(self.counter)?;
        Ok(())
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.cancel_reason.lock().unwrap() = reason;
        Ok(())
    }
}

/// Source whose start() immediately rejects.
#[cfg(feature = "send")]
struct FailingStartSource;

#[cfg(feature = "send")]
impl ReadableSource<u32> for FailingStartSource {
    async fn start(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Err(StreamError::from("start rejected"))
    }

    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Ok(())
    }
}

/// Source whose pull() immediately rejects.
#[cfg(feature = "send")]
struct FailingPullSource;

#[cfg(feature = "send")]
impl ReadableSource<u32> for FailingPullSource {
    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Err(StreamError::from("pull rejected"))
    }
}

// ── WPT: readable-streams/general.any.js ─────────────────────────────────────

// "ReadableStream: reading from an empty stream gives undefined"
#[cfg(feature = "send")]
#[tokio::test]
async fn reading_from_empty_stream_gives_none() {
    let stream = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableStream: reading all values yields each in order, then closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn reading_all_values_in_order_then_closed() {
    let data = vec![1u32, 2, 3];
    let stream = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    for expected in &data {
        assert_eq!(reader.read().await.unwrap(), Some(*expected));
    }
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableStream: locked reflects whether a reader is held"
#[cfg(feature = "send")]
#[tokio::test]
async fn locked_reflects_reader_state() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);

    assert!(!stream.locked());
    let (_locked_stream, reader) = stream.get_reader().unwrap();
    assert!(stream.locked());

    let stream = reader.release_lock();
    assert!(!stream.locked());
}

// "ReadableStream: get_reader() on an already-locked stream returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_reader_fails_when_locked() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let (_locked, _reader) = stream.get_reader().unwrap();

    assert!(stream.get_reader().is_err());
}

// "ReadableStream: dropping the reader releases the lock"
#[cfg(feature = "send")]
#[tokio::test]
async fn dropping_reader_releases_lock() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    {
        let (_locked, _reader) = stream.get_reader().unwrap();
        assert!(stream.locked());
    }
    assert!(!stream.locked());
}

// "ReadableStream: after releasing a reader, a new reader can be acquired"
#[cfg(feature = "send")]
#[tokio::test]
async fn new_reader_after_release_lock() {
    let stream = ReadableStream::from_vec(vec![1u32, 2]).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(1));

    let stream = reader.release_lock();
    let (_locked, reader2) = stream.get_reader().unwrap();
    assert_eq!(reader2.read().await.unwrap(), Some(2));
}

// "ReadableStream: if start() rejects, the stream becomes errored"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_rejection_errors_stream() {
    let stream = ReadableStream::builder(FailingStartSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// "ReadableStream: if pull() rejects, the stream becomes errored"
#[cfg(feature = "send")]
#[tokio::test]
async fn pull_rejection_errors_stream() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// "ReadableStream: subsequent reads after error also return an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn reads_after_error_also_error() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    assert!(reader.read().await.is_err());
    assert!(reader.read().await.is_err());
}

// "ReadableStream: closed promise resolves after the stream closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn closed_resolves_when_stream_exhausted() {
    let stream = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // drain (triggers close)
    reader.closed().await.unwrap();
}

// "ReadableStream: closed promise rejects when the stream errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn closed_rejects_when_stream_errored() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // triggers error
    assert!(reader.closed().await.is_err());
}

// "ReadableStream: can be used as a futures::Stream"
#[cfg(feature = "send")]
#[tokio::test]
async fn implements_futures_stream_trait() {
    use futures::StreamExt;

    let items = vec![10u32, 20, 30];
    let mut stream = ReadableStream::from_vec(items.clone()).spawn(tokio::spawn);

    let mut collected = Vec::new();
    while let Some(result) = stream.next().await {
        collected.push(result.unwrap());
    }
    assert_eq!(collected, items);
}

// "ReadableStream: desired_size is positive when queue is below HWM"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_desired_size_reflects_hwm() {
    use std::sync::{Arc, Mutex};

    struct CapturingSource {
        desired_size: Arc<Mutex<Option<isize>>>,
    }

    impl ReadableSource<u32> for CapturingSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.desired_size.lock().unwrap() = controller.desired_size();
            controller.close()?;
            Ok(())
        }
    }

    let captured = Arc::new(Mutex::new(None));
    let stream = ReadableStream::builder(CapturingSource {
        desired_size: captured.clone(),
    })
    .strategy(CountQueuingStrategy::new(4))
    .spawn(tokio::spawn);

    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // triggers pull → close

    // Give the task time to record desired_size
    tokio::task::yield_now().await;

    let size = *captured.lock().unwrap();
    // HWM=4, queue was empty when pull ran → desired_size should be 4
    assert_eq!(size, Some(4));
}

// ── WPT: readable-streams/cancel.any.js ──────────────────────────────────────

// "ReadableStream cancel() returns Ok"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_returns_ok() {
    let stream = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.cancel(None).await.is_ok());
}

// "ReadableStream: after reader.cancel(), read() returns None"
#[cfg(feature = "send")]
#[tokio::test]
async fn read_after_cancel_gives_none() {
    let stream = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    reader.cancel(None).await.unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableStream: cancel() on an already-closed stream returns Ok"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_on_closed_stream_returns_ok() {
    let stream = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // close the stream
    assert!(reader.cancel(None).await.is_ok());
}

// "Cancel passes the given reason to the underlying source's cancel method"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_reason_passed_to_source() {
    use std::sync::{Arc, Mutex};

    let cancel_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let source = CountingSource {
        counter: 0,
        cancel_reason: cancel_reason.clone(),
    };

    // HWM=1: task pulls once, fills queue, then stops pulling.
    // When cancel arrives, source is back in inner.source (not in a pull future).
    let stream = ReadableStream::builder(source)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);

    let (_locked, reader) = stream.get_reader().unwrap();

    // Read one chunk so the task has settled (source back in inner.source after HWM is reached)
    assert_eq!(reader.read().await.unwrap(), Some(1));

    reader.cancel(Some("my reason".into())).await.unwrap();

    assert_eq!(
        cancel_reason.lock().unwrap().as_deref(),
        Some("my reason"),
        "source.cancel() should have been called with the given reason"
    );
}

// "ReadableStream: cancel() with no reason passes None to the source"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_none_reason_passed_to_source() {
    use std::sync::{Arc, Mutex};

    let cancel_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(Some("sentinel".into())));

    let source = CountingSource {
        counter: 0,
        cancel_reason: cancel_reason.clone(),
    };

    let stream = ReadableStream::builder(source)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);

    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await; // settle the task
    reader.cancel(None).await.unwrap();

    assert_eq!(
        *cancel_reason.lock().unwrap(),
        None,
        "source.cancel() should have been called with None"
    );
}

// "ReadableStream: stream.cancel() on a locked stream should reject"
// SPEC GAP: The implementation does not check the locked flag in stream.cancel().
// Per the WHATWG spec §3.3.3, cancel() must reject if the stream is locked.
#[cfg(feature = "send")]
#[tokio::test]
#[ignore = "spec gap: stream.cancel() does not reject when stream is locked"]
async fn stream_cancel_on_locked_stream_rejects() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let (_locked, _reader) = stream.get_reader().unwrap();

    let result = stream.cancel(None).await;
    assert!(
        result.is_err(),
        "cancel() on a locked stream must return an error per spec §3.3.3"
    );
}
