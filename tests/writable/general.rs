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
