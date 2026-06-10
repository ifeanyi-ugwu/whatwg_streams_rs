// WPT: streams/writable-streams/general.any.js

use crate::helpers::LifecycleSink;
use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

struct FailingStartSink;

impl WritableSink<u32> for FailingStartSink {
    async fn start(
        &mut self,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Err("start rejected".into())
    }

    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Ok(())
    }
}

// "WritableStream: initial state should be writable (not locked)"
#[cfg(feature = "send")]
#[tokio::test]
async fn initial_state_is_not_locked() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    assert!(!stream.locked());
}

// "WritableStream: get_writer() on an unlocked stream succeeds"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_succeeds_when_unlocked() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, _writer) = stream.get_writer().expect("get_writer should succeed");
    assert!(stream.locked());
}

// "WritableStream: get_writer() on an already-locked stream returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_fails_when_locked() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, _writer1) = stream.get_writer().unwrap();
    assert!(stream.get_writer().is_err());
}

// "WritableStream: dropping the writer releases the lock"
#[cfg(feature = "send")]
#[tokio::test]
async fn dropping_writer_releases_lock() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    {
        let (_locked, _writer) = stream.get_writer().unwrap();
        assert!(stream.locked());
    }
    assert!(!stream.locked());
}

// "WritableStream: if start() rejects, writes are rejected"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_rejection_rejects_writes() {
    let stream = WritableStream::builder(FailingStartSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.write(1u32).await.is_err());
}

// ── WPT gaps: writable-streams/general.any.js ────────────────────────────────

// "desiredSize initial value"
// writer.desired_size() must equal HWM (default 1) when the queue is empty.
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_initial_value() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert_eq!(writer.desired_size(), Some(1), "desiredSize must equal HWM=1 initially");
}

// "desiredSize on a writer for a closed stream"
// After stream closes, desiredSize must be 0 (not null/None).
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_is_zero_on_closed_stream() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.close().await.unwrap();
    assert_eq!(
        writer.desired_size(),
        Some(0),
        "desiredSize must be 0 (not None) after stream is closed"
    );
}

// "ws.getWriter() on a closing WritableStream"
// After release_lock() (JS: writer.releaseLock()), getWriter() must succeed
// even when a close was previously initiated. The lock is tied to the writer's
// lifetime in Rust, so release_lock() is the idiomatic equivalent.
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_on_closing_stream_succeeds() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    // release_lock() explicitly frees the lock (matches JS writer.releaseLock())
    writer.release_lock().unwrap();
    assert!(
        stream.get_writer().is_ok(),
        "get_writer() must succeed after release_lock()"
    );
}

// "ws.getWriter() on a closed WritableStream"
// After stream closes and lock is released, getWriter() must succeed.
#[cfg(feature = "send")]
#[tokio::test]
async fn get_writer_on_closed_stream_succeeds() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    {
        let (_locked, writer) = stream.get_writer().unwrap();
        writer.close().await.unwrap();
        // _locked drops → lock released
    }
    assert!(stream.get_writer().is_ok(), "get_writer() must succeed on an already-closed stream");
}
