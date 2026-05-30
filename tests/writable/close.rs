// WPT: streams/writable-streams/close.any.js

use crate::helpers::{CollectSink, LifecycleSink};
use whatwg_streams::{CountQueuingStrategy, WritableStream};

// "Closing a WritableStream calls close() on the underlying sink"
#[cfg(feature = "send")]
#[tokio::test]
async fn close_calls_sink_close() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = LifecycleSink {
        closed: closed.clone(),
        ..Default::default()
    };
    let stream = WritableStream::builder(sink).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.close().await.unwrap();
    assert!(*closed.lock().unwrap());
}

// "Closing a WritableStream fulfills the writer's closed promise"
#[cfg(feature = "send")]
#[tokio::test]
async fn close_fulfills_writer_closed_promise() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.close().await.unwrap();
    writer.closed().await.unwrap();
}

// "Writes before close() all land in the sink before close() is called"
#[cfg(feature = "send")]
#[tokio::test]
async fn close_drains_writes_first() {
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    writer.write(1u32).await.unwrap();
    writer.write(2u32).await.unwrap();
    writer.write(3u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(*collected.lock().unwrap(), vec![1, 2, 3]);
}

// "WritableStream: write() after close() returns an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn write_after_close_errors() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.close().await.unwrap();
    assert!(writer.write(99u32).await.is_err());
}
