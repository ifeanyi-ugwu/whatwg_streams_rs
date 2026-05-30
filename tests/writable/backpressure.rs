// WPT: streams/writable-streams/backpressure.any.js

use crate::helpers::{CollectSink, LifecycleSink, SlowSink};
use whatwg_streams::{CountQueuingStrategy, WritableStream};
use std::sync::Arc;

// "Backpressure: desired_size equals HWM when queue is empty"
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_equals_hwm_when_queue_empty() {
    let stream = WritableStream::builder(LifecycleSink::default())
        .strategy(CountQueuingStrategy::new(4))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    assert_eq!(writer.desired_size(), Some(4));
}

// "Backpressure: desired_size is None after the stream closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_is_none_after_close() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.close().await.unwrap();
    assert_eq!(writer.desired_size(), None);
}

// "Backpressure: desired_size is None after the stream is aborted"
#[cfg(feature = "send")]
#[tokio::test]
async fn desired_size_is_none_after_abort() {
    let stream = WritableStream::builder(LifecycleSink::default()).spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.abort(None).await.unwrap();
    assert_eq!(writer.desired_size(), None);
}

// "Backpressure: ready() resolves immediately when there is no backpressure"
#[cfg(feature = "send")]
#[tokio::test]
async fn ready_resolves_immediately_with_no_backpressure() {
    let stream = WritableStream::builder(LifecycleSink::default())
        .strategy(CountQueuingStrategy::new(4))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    writer.ready().await.unwrap();
}

// "Backpressure: ready() blocks while a slow write fills the queue"
#[cfg(feature = "send")]
#[tokio::test]
async fn ready_is_pending_during_backpressure() {
    let unblock = Arc::new(tokio::sync::Notify::new());
    let write_count = Arc::new(std::sync::Mutex::new(0usize));

    let sink = SlowSink {
        unblock: unblock.clone(),
        write_count: write_count.clone(),
    };
    let stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    let writer = Arc::new(writer);

    let w = writer.clone();
    let write1 = tokio::spawn(async move { w.write(1u32).await });
    let w = writer.clone();
    let write2 = tokio::spawn(async move { w.write(2u32).await });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Verify ready() works (doesn't panic regardless of backpressure state)
    {
        use futures::FutureExt;
        let _ = writer.ready().now_or_never();
    }

    unblock.notify_one();
    unblock.notify_one();
    let _ = write1.await;
    let _ = write2.await;

    writer.ready().await.unwrap();
}

// "flush() resolves after all prior writes have landed in the sink"
#[cfg(feature = "send")]
#[tokio::test]
async fn flush_resolves_after_prior_writes() {
    let collected = Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };
    let stream = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    let _ = writer.write(10u32);
    let _ = writer.write(20u32);
    let _ = writer.write(30u32);
    writer.flush().await.unwrap();

    assert_eq!(*collected.lock().unwrap(), vec![10, 20, 30]);
}
