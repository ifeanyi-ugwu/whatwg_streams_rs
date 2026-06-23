// WPT: streams/queuing-strategies.any.js
// https://github.com/web-platform-tests/wpt/blob/master/streams/queuing-strategies.any.js

use whatwg_streams::{
    ByteLengthQueuingStrategy, CountQueuingStrategy, QueuingStrategy, ReadableStream,
    WritableStream,
};

// ── CountQueuingStrategy ──────────────────────────────────────────────────────

// "CountQueuingStrategy: has the correct high water mark"
#[cfg(feature = "send")]
#[test]
fn count_strategy_high_water_mark() {
    let s = CountQueuingStrategy::new(4);
    assert_eq!(<CountQueuingStrategy as QueuingStrategy<u32>>::high_water_mark(&s), 4);
}

// "CountQueuingStrategy: chunk size is always 1"
#[cfg(feature = "send")]
#[test]
fn count_strategy_size_is_always_one() {
    let s = CountQueuingStrategy::new(1);
    assert_eq!(s.size(&42u32), 1);
    assert_eq!(s.size(&vec![1u8, 2, 3, 4, 5]), 1);
    assert_eq!(s.size(&"hello"), 1);
}

// "CountQueuingStrategy: high water mark of 0 creates instant backpressure"
#[cfg(feature = "send")]
#[tokio::test]
async fn count_strategy_hwm_zero_instant_backpressure() {
    use whatwg_streams::{WritableSink, WritableStreamDefaultController, StreamResult};

    struct NullSink;
    impl WritableSink<u32> for NullSink {
        async fn write(&mut self, _chunk: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            Ok(())
        }
    }

    let stream = WritableStream::builder(NullSink)
        .strategy(CountQueuingStrategy::new(0))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();
    // With HWM=0, desired_size starts at 0 (no room)
    assert_eq!(writer.desired_size(), Some(0));
}

// "CountQueuingStrategy: backpressure clears after each chunk is processed"
#[cfg(feature = "send")]
#[tokio::test]
async fn count_strategy_backpressure_clears_after_write() {
    use whatwg_streams::{WritableSink, WritableStreamDefaultController, StreamResult};

    struct NullSink;
    impl WritableSink<u32> for NullSink {
        async fn write(&mut self, _chunk: u32, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            Ok(())
        }
    }

    let stream = WritableStream::builder(NullSink)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    assert_eq!(writer.desired_size(), Some(1));
    writer.write(1u32).await.unwrap();
    // After write completes, queue is empty again → desired_size back to HWM
    assert_eq!(writer.desired_size(), Some(1));
}

// "CountQueuingStrategy: used as readable strategy, controls how many chunks are buffered"
#[cfg(feature = "send")]
#[tokio::test]
async fn count_strategy_on_readable_limits_buffer() {
    let stream = ReadableStream::from_vec(vec![1u32, 2, 3, 4, 5])
        .strategy(CountQueuingStrategy::new(2))
        .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    let mut result = Vec::new();
    while let Some(v) = reader.read().await.unwrap() {
        result.push(v);
    }
    assert_eq!(result, vec![1, 2, 3, 4, 5]);
}

// ── ByteLengthQueuingStrategy ─────────────────────────────────────────────────

// "ByteLengthQueuingStrategy: has the correct high water mark"
#[cfg(feature = "send")]
#[test]
fn byte_length_strategy_high_water_mark() {
    let s = ByteLengthQueuingStrategy::new(65536);
    assert_eq!(<ByteLengthQueuingStrategy as QueuingStrategy<Vec<u8>>>::high_water_mark(&s), 65536);
}

// "ByteLengthQueuingStrategy: chunk size equals byte length of the chunk"
#[cfg(feature = "send")]
#[test]
fn byte_length_strategy_size_equals_chunk_len() {
    let s = ByteLengthQueuingStrategy::new(1024);
    assert_eq!(s.size(&vec![0u8; 0]), 0);
    assert_eq!(s.size(&vec![0u8; 1]), 1);
    assert_eq!(s.size(&vec![0u8; 512]), 512);
}

// "ByteLengthQueuingStrategy: works as a writable strategy"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_length_strategy_on_writable() {
    use crate::helpers::CollectSink;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Vec<u8>>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };

    // HWM = 8 bytes
    let stream = WritableStream::builder(sink)
        .strategy(ByteLengthQueuingStrategy::new(8))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    // desired_size starts at HWM (8)
    assert_eq!(writer.desired_size(), Some(8));

    writer.write(vec![0u8; 4]).await.unwrap();
    writer.write(vec![0u8; 4]).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(collected.lock().unwrap().len(), 2);
}

// "ByteLengthQueuingStrategy: desired_size decreases by chunk byte length"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_length_strategy_desired_size_tracks_bytes() {
    use whatwg_streams::{WritableSink, WritableStreamDefaultController, StreamResult};
    use std::sync::{Arc, Mutex};

    // A sink that blocks after the first write so we can inspect desired_size mid-flight
    let unblock = Arc::new(tokio::sync::Notify::new());
    let unblock2 = unblock.clone();

    struct HoldingSink { unblock: Arc<tokio::sync::Notify>, first: bool }
    impl WritableSink<Vec<u8>> for HoldingSink {
        async fn write(&mut self, _chunk: Vec<u8>, _: &mut WritableStreamDefaultController) -> StreamResult<()> {
            if self.first {
                self.first = false;
                self.unblock.notified().await;
            }
            Ok(())
        }
    }

    let stream = WritableStream::builder(HoldingSink { unblock, first: true })
        .strategy(ByteLengthQueuingStrategy::new(16))
        .spawn(tokio::spawn);
    let (_locked, writer) = stream.get_writer().unwrap();

    assert_eq!(writer.desired_size(), Some(16));

    // Start a write of 10 bytes (goes inflight, sink blocks)
    let w = std::sync::Arc::new(writer);
    let wc = w.clone();
    let write_fut = tokio::spawn(async move { wc.write(vec![0u8; 10]).await });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Unblock the sink and wait for write to complete
    unblock2.notify_one();
    write_fut.await.unwrap().unwrap();

    // After write completes the queue is drained → desired_size back to HWM
    assert_eq!(w.desired_size(), Some(16));
}

// "ByteLengthQueuingStrategy: works as a readable strategy"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_length_strategy_on_readable() {
    use whatwg_streams::{ReadableSource, ReadableStreamDefaultController, StreamResult};

    struct ByteSource { chunks: Vec<Vec<u8>>, i: usize }
    impl ReadableSource<Vec<u8>> for ByteSource {
        async fn pull(&mut self, controller: &mut ReadableStreamDefaultController<Vec<u8>>) -> StreamResult<()> {
            if self.i < self.chunks.len() {
                controller.enqueue(self.chunks[self.i].clone())?;
                self.i += 1;
            } else {
                controller.close()?;
            }
            Ok(())
        }
    }

    let source = ByteSource {
        chunks: vec![vec![0u8; 100], vec![0u8; 200]],
        i: 0,
    };

    // HWM = 256 bytes: both chunks (100+200=300) exceed it, but each individually fits
    let stream = ReadableStream::builder(source)
        .strategy(ByteLengthQueuingStrategy::new(256))
        .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    let c1 = reader.read().await.unwrap().unwrap();
    let c2 = reader.read().await.unwrap().unwrap();
    assert_eq!(c1.len(), 100);
    assert_eq!(c2.len(), 200);
    assert_eq!(reader.read().await.unwrap(), None);
}
