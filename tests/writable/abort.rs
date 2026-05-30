// WPT: streams/writable-streams/abort.any.js

use crate::helpers::LifecycleSink;
use whatwg_streams::{StreamResult, WritableSink, WritableStream, WritableStreamDefaultController};

struct FailingWriteSink;

impl WritableSink<u32> for FailingWriteSink {
    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        Err("write rejected".into())
    }
}

// "Aborting a WritableStream calls abort() on the underlying sink with the reason"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_calls_sink_abort_with_reason() {
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = LifecycleSink {
        aborted: aborted.clone(),
        ..Default::default()
    };
    let stream = WritableStream::builder(sink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("abort reason".into())).await.unwrap();
    assert_eq!(aborted.lock().unwrap().as_deref(), Some("abort reason"));
}

// "Aborting a WritableStream immediately prevents further writes"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_prevents_further_writes() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(None).await.unwrap();
    assert!(writer.write(1u32).await.is_err());
}

// "WritableStream: write() after abort() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_after_abort_errors() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(Some("reason".into())).await.unwrap();
    assert!(writer.write(42u32).await.is_err());
}

// "WritableStream: close() after abort() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn close_after_abort_errors() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(None).await.unwrap();
    assert!(writer.close().await.is_err());
}

// "WritableStream: if write() rejects, subsequent writes also fail"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_rejection_errors_stream() {
    let stream = WritableStream::builder(FailingWriteSink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert!(writer.write(1u32).await.is_err());
    assert!(writer.write(2u32).await.is_err());
}
