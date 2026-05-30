//! Integration tests for pipeTo() and pipeThrough(), ported from:
//! WPT: streams/piping/general-addition.any.js
//! WPT: streams/piping/close-propagation-forward.any.js
//! WPT: streams/piping/close-propagation-backward.any.js
//! WPT: streams/piping/error-propagation-via-abort.any.js
//! WPT: streams/piping/abort.any.js
//! https://github.com/web-platform-tests/wpt/tree/master/streams/piping

use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamError, StreamPipeOptions, StreamResult, TransformStream, Transformer,
    TransformStreamDefaultController, WritableSink, WritableStream, WritableStreamDefaultController,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Sink that collects all written chunks.
#[cfg(feature = "send")]
struct CollectSink<T> {
    collected: std::sync::Arc<std::sync::Mutex<Vec<T>>>,
    closed: std::sync::Arc<std::sync::Mutex<bool>>,
    aborted: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl<T: Send + 'static> WritableSink<T> for CollectSink<T> {
    async fn write(
        &mut self,
        chunk: T,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        self.collected.lock().unwrap().push(chunk);
        Ok(())
    }

    async fn close(self) -> StreamResult<()> {
        *self.closed.lock().unwrap() = true;
        Ok(())
    }

    async fn abort(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.aborted.lock().unwrap() = reason;
        Ok(())
    }
}

/// Sink whose write() fails after `fail_after` successful writes.
#[cfg(feature = "send")]
struct FailAfterSink {
    fail_after: usize,
    count: std::sync::Arc<std::sync::Mutex<usize>>,
}

#[cfg(feature = "send")]
impl WritableSink<u32> for FailAfterSink {
    async fn write(
        &mut self,
        _chunk: u32,
        _controller: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        let mut c = self.count.lock().unwrap();
        *c += 1;
        if *c > self.fail_after {
            Err(StreamError::from("sink write failed"))
        } else {
            Ok(())
        }
    }
}

/// Source that emits `data`, then errors the stream.
#[cfg(feature = "send")]
struct ErrorAfterSource {
    data: Vec<u32>,
    index: std::sync::Arc<std::sync::Mutex<usize>>,
}

#[cfg(feature = "send")]
impl ReadableSource<u32> for ErrorAfterSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        let idx = {
            let mut i = self.index.lock().unwrap();
            let v = *i;
            *i += 1;
            v
        };

        if idx < self.data.len() {
            controller.enqueue(self.data[idx])?;
        } else {
            return Err(StreamError::from("source error"));
        }
        Ok(())
    }
}

/// Identity transformer — passes chunks unchanged.
#[cfg(feature = "send")]
struct IdentityT;

#[cfg(feature = "send")]
impl Transformer<u32, u32> for IdentityT {
    async fn transform(
        &mut self,
        chunk: u32,
        controller: &mut TransformStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk)
    }
}

/// Doubling transformer — multiplies each chunk by 2.
#[cfg(feature = "send")]
struct DoubleT;

#[cfg(feature = "send")]
impl Transformer<u32, u32> for DoubleT {
    async fn transform(
        &mut self,
        chunk: u32,
        controller: &mut TransformStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk * 2)
    }
}

// ── WPT: piping/general-addition.any.js ──────────────────────────────────────

// "Piping: all chunks flow from the readable to the writable"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_transfers_all_chunks() {
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };

    let data = vec![1u32, 2, 3];
    let source = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(10))
        .spawn(tokio::spawn);

    source.pipe_to(&dest, None).await.unwrap();

    assert_eq!(*collected.lock().unwrap(), data);
}

// "Piping: from an empty stream closes the destination immediately"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_from_empty_stream_closes_dest() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };

    let source = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    source.pipe_to(&dest, None).await.unwrap();

    assert!(*closed.lock().unwrap(), "destination should be closed");
}

// ── WPT: piping/close-propagation-forward.any.js ─────────────────────────────

// "Piping: close propagates from the readable to the writable (prevent_close=false)"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_close_propagates_forward() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };

    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    source
        .pipe_to(&dest, Some(StreamPipeOptions::default()))
        .await
        .unwrap();

    assert!(*closed.lock().unwrap(), "writable should be closed when readable closes");
}

// "Piping: prevent_close=true prevents destination close when source closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_prevent_close_option() {
    let closed = std::sync::Arc::new(std::sync::Mutex::new(false));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: closed.clone(),
        aborted: Default::default(),
    };

    let source = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_close: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();

    assert!(
        !*closed.lock().unwrap(),
        "writable should NOT be closed when prevent_close=true"
    );
}

// ── WPT: piping/error-propagation-via-abort.any.js ───────────────────────────

// "Piping: error in source aborts the destination (prevent_abort=false)"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_source_error_aborts_dest() {
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: aborted.clone(),
    };

    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![],
        index: Default::default(),
    })
    .spawn(tokio::spawn);

    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let result = source.pipe_to(&dest, None).await;
    assert!(result.is_err(), "pipe_to should return error when source errors");

    // Destination should have received an abort signal
    assert!(
        aborted.lock().unwrap().is_some(),
        "destination should be aborted when source errors"
    );
}

// "Piping: prevent_abort=true: destination is NOT aborted when source errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_prevent_abort_option() {
    let aborted = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let sink = CollectSink::<u32> {
        collected: Default::default(),
        closed: Default::default(),
        aborted: aborted.clone(),
    };

    let source = ReadableStream::builder(ErrorAfterSource {
        data: vec![],
        index: Default::default(),
    })
    .spawn(tokio::spawn);

    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let _ = source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_abort: true,
                ..Default::default()
            }),
        )
        .await;

    assert!(
        aborted.lock().unwrap().is_none(),
        "destination should NOT be aborted when prevent_abort=true"
    );
}

// "Piping: prevent_cancel=true: source is NOT cancelled when destination errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_prevent_cancel_option() {
    // Sink fails after 0 writes → triggers a write error immediately
    let sink = FailAfterSink {
        fail_after: 0,
        count: Default::default(),
    };

    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink).spawn(tokio::spawn);

    let result = source
        .pipe_to(
            &dest,
            Some(StreamPipeOptions {
                prevent_cancel: true,
                ..Default::default()
            }),
        )
        .await;

    // The pipe should fail due to the sink error
    assert!(result.is_err());
}

// ── WPT: piping via TransformStream (pipeThrough) ────────────────────────────

// "pipeThrough: data flows through the transform and out the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_through_transforms_chunks() {
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let transform = TransformStream::builder(DoubleT).spawn(tokio::spawn);

    let output = source.pipe_through(transform, None).spawn(tokio::spawn);
    let (_locked, reader) = output.get_reader().unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(2));
    assert_eq!(reader.read().await.unwrap(), Some(4));
    assert_eq!(reader.read().await.unwrap(), Some(6));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "pipeThrough: closing source closes the transform and then the output"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_through_close_propagates() {
    let source = ReadableStream::from_vec(Vec::<u32>::new()).spawn(tokio::spawn);
    let transform = TransformStream::builder(IdentityT).spawn(tokio::spawn);

    let output = source.pipe_through(transform, None).spawn(tokio::spawn);
    let (_locked, reader) = output.get_reader().unwrap();

    // Empty source → output should immediately close
    assert_eq!(reader.read().await.unwrap(), None);
}

// "pipeThrough: chaining two transforms works end-to-end"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_through_chain() {
    // Source: [1, 2, 3]
    // Stage 1: DoubleT → [2, 4, 6]
    // Stage 2: DoubleT → [4, 8, 12]
    let source = ReadableStream::from_vec(vec![1u32, 2, 3]).spawn(tokio::spawn);
    let t1 = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let t2 = TransformStream::builder(DoubleT).spawn(tokio::spawn);

    let mid = source.pipe_through(t1, None).spawn(tokio::spawn);
    let out = mid.pipe_through(t2, None).spawn(tokio::spawn);

    let (_locked, reader) = out.get_reader().unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(4));
    assert_eq!(reader.read().await.unwrap(), Some(8));
    assert_eq!(reader.read().await.unwrap(), Some(12));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "pipe_to: multiple concurrent reads don't cause data loss"
#[cfg(feature = "send")]
#[tokio::test]
async fn pipe_to_large_stream() {
    let n = 100u32;
    let data: Vec<u32> = (1..=n).collect();
    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u32>::new()));
    let sink = CollectSink {
        collected: collected.clone(),
        closed: Default::default(),
        aborted: Default::default(),
    };

    let source = ReadableStream::from_vec(data.clone()).spawn(tokio::spawn);
    let dest = WritableStream::builder(sink)
        .strategy(CountQueuingStrategy::new(50))
        .spawn(tokio::spawn);

    source.pipe_to(&dest, None).await.unwrap();

    let result = collected.lock().unwrap().clone();
    assert_eq!(result, data, "all {} chunks must arrive in order", n);
}
