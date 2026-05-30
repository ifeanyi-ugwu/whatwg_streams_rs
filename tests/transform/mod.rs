// WPT: streams/transform-streams/

use whatwg_streams::{
    StreamResult, TransformStream, TransformStreamDefaultController, Transformer,
};

struct DoubleT;

impl Transformer<u32, u32> for DoubleT {
    async fn transform(
        &mut self,
        chunk: u32,
        controller: &mut TransformStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk * 2)
    }
}

struct UppercaseT;

impl Transformer<String, String> for UppercaseT {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        controller.enqueue(chunk.to_uppercase())
    }
}

struct FilterOddT;

impl Transformer<i32, i32> for FilterOddT {
    async fn transform(
        &mut self,
        chunk: i32,
        controller: &mut TransformStreamDefaultController<i32>,
    ) -> StreamResult<()> {
        if chunk % 2 != 0 {
            controller.enqueue(chunk)?;
        }
        Ok(())
    }
}

struct ErrorOnThreeT;

impl Transformer<i32, i32> for ErrorOnThreeT {
    async fn transform(
        &mut self,
        chunk: i32,
        controller: &mut TransformStreamDefaultController<i32>,
    ) -> StreamResult<()> {
        if chunk == 3 {
            Err("cannot process 3".into())
        } else {
            controller.enqueue(chunk)
        }
    }
}

// WPT: transform-streams/general.any.js —
// "TransformStream: enqueued chunks appear on the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_passes_chunks_through() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    writer.write(2u32).await.unwrap();
    writer.write(3u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(2));
    assert_eq!(reader.read().await.unwrap(), Some(4));
    assert_eq!(reader.read().await.unwrap(), Some(6));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: transform can change the type of chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_changes_chunk_type() {
    let ts = TransformStream::builder(UppercaseT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write("hello".to_string()).await.unwrap();
    writer.write("world".to_string()).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some("HELLO".to_string()));
    assert_eq!(reader.read().await.unwrap(), Some("WORLD".to_string()));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: transform can filter out chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_can_filter_chunks() {
    let ts = TransformStream::builder(FilterOddT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    for i in 1i32..=5 {
        writer.write(i).await.unwrap();
    }
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(3));
    assert_eq!(reader.read().await.unwrap(), Some(5));
    assert_eq!(reader.read().await.unwrap(), None);
}

// WPT: transform-streams/general.any.js —
// "TransformStream: closing the writable side closes the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn closing_writable_closes_readable() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.close().await.unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// WPT: transform-streams/errors.any.js —
// "TransformStream: error in transform() errors both sides"
#[cfg(feature = "send")]
#[tokio::test]
async fn transform_error_errors_both_sides() {
    let ts = TransformStream::builder(ErrorOnThreeT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1i32).await.unwrap();
    writer.write(2i32).await.unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(2));

    // This write triggers the error
    assert!(writer.write(3i32).await.is_err());
    // Readable side is now errored too
    assert!(reader.read().await.is_err());
}

// WPT: transform-streams/general.any.js —
// "TransformStream: abort() on the writable errors the readable"
#[cfg(feature = "send")]
#[tokio::test]
async fn abort_writable_errors_readable() {
    let ts = TransformStream::builder(DoubleT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(2));

    // Abort signals an error on the writable side, which errors the readable too
    let abort_result = writer.abort(Some("aborted".into())).await;
    assert!(abort_result.is_err()); // abort itself returns the error

    assert!(reader.read().await.is_err());
}

// WPT: transform-streams/flush.any.js —
// "TransformStream: flush() is called when the writable closes"
#[cfg(feature = "send")]
#[tokio::test]
async fn flush_called_on_writable_close() {
    use std::sync::{Arc, Mutex};

    struct FlushTracker {
        flushed: Arc<Mutex<bool>>,
    }

    impl Transformer<u32, u32> for FlushTracker {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn flush(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.flushed.lock().unwrap() = true;
            controller.enqueue(999u32)?; // sentinel chunk emitted in flush
            Ok(())
        }
    }

    let flushed = Arc::new(Mutex::new(false));
    let ts = TransformStream::builder(FlushTracker {
        flushed: flushed.clone(),
    })
    .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(999)); // flush sentinel
    assert_eq!(reader.read().await.unwrap(), None);
    assert!(*flushed.lock().unwrap());
}

// ── WPT: transform-streams/terminate.any.js ──────────────────────────────────
// https://github.com/web-platform-tests/wpt/blob/master/streams/transform-streams/terminate.any.js

// "TransformStream: controller.terminate() closes the readable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_closes_readable_side() {
    struct TerminatingT;

    impl Transformer<u32, u32> for TerminatingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)?;
            controller.terminate()?; // close readable, error writable
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminatingT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();

    // First read returns the enqueued chunk
    assert_eq!(reader.read().await.unwrap(), Some(1));
    // Second read returns None — readable is closed
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: controller.terminate() errors the writable side"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_errors_writable_side() {
    struct TerminatingT;

    impl Transformer<u32, u32> for TerminatingT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)?;
            controller.terminate()?;
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminatingT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // First write triggers terminate() — readable gets the chunk, writable errors
    writer.write(1u32).await.unwrap();
    let _ = reader.read().await; // consume the chunk

    // Subsequent writes on the writable should now fail
    assert!(
        writer.write(2u32).await.is_err(),
        "write() after terminate() must return an error"
    );
}

// "TransformStream: controller.terminate() called in start() closes immediately"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_in_start_closes_immediately() {
    struct TerminateOnStartT;

    impl Transformer<u32, u32> for TerminateOnStartT {
        async fn start(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.terminate()?;
            Ok(())
        }

        async fn transform(
            &mut self,
            _chunk: u32,
            _controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminateOnStartT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    // Readable is already closed — reads return None immediately
    assert_eq!(reader.read().await.unwrap(), None);
    // Writable is errored — writes fail
    assert!(writer.write(1u32).await.is_err());
}

// "TransformStream: terminate() in flush() closes the readable after all chunks"
#[cfg(feature = "send")]
#[tokio::test]
async fn terminate_in_flush_closes_after_chunks() {
    struct TerminateInFlushT;

    impl Transformer<u32, u32> for TerminateInFlushT {
        async fn transform(
            &mut self,
            chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            controller.enqueue(chunk)
        }

        async fn flush(
            &mut self,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // terminate() in flush: readable closes, no sentinel chunk needed
            controller.terminate()?;
            Ok(())
        }
    }

    let ts = TransformStream::builder(TerminateInFlushT).spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    writer.write(2u32).await.unwrap();
    writer.close().await.unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(1));
    assert_eq!(reader.read().await.unwrap(), Some(2));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "TransformStream: desired_size on the controller reflects the readable side's HWM"
#[cfg(feature = "send")]
#[tokio::test]
async fn controller_desired_size_reflects_readable_hwm() {
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Option<isize>>> = Arc::new(Mutex::new(None));
    let captured2 = captured.clone();

    struct CapturingT {
        captured: Arc<Mutex<Option<isize>>>,
    }

    impl Transformer<u32, u32> for CapturingT {
        async fn transform(
            &mut self,
            _chunk: u32,
            controller: &mut TransformStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            *self.captured.lock().unwrap() = controller.desired_size();
            controller.terminate()?;
            Ok(())
        }
    }

    use whatwg_streams::CountQueuingStrategy;
    let ts = TransformStream::builder(CapturingT { captured: captured2 })
        .readable_strategy(CountQueuingStrategy::new(4))
        .spawn(tokio::spawn);
    let (readable, writable) = ts.split();
    let (_locked, writer) = writable.get_writer().unwrap();
    let (_locked, _reader) = readable.get_reader().unwrap();

    writer.write(1u32).await.unwrap();
    tokio::task::yield_now().await;

    // Readable HWM=4, queue was empty when transform ran → desired_size = 4
    assert_eq!(*captured.lock().unwrap(), Some(4));
}
