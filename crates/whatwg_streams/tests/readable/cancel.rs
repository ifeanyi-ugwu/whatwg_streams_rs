// WPT: streams/readable-streams/cancel.any.js

use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamResult,
};

struct CountingSource {
    counter: u32,
    cancel_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

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
    let _ = reader.read().await;
    assert!(reader.cancel(None).await.is_ok());
}

// "Cancel passes the given reason to the underlying source's cancel method"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_reason_passed_to_source() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
    let source = CountingSource {
        counter: 0,
        cancel_reason: cancel_reason.clone(),
    };
    let stream = ReadableStream::builder(source)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(1));
    reader.cancel(Some("my reason".into())).await.unwrap();
    assert_eq!(cancel_reason.lock().unwrap().as_deref(), Some("my reason"));
}

// "ReadableStream: cancel() with no reason passes None to the source"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_none_reason_passed_to_source() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(Some("sentinel".into())));
    let source = CountingSource {
        counter: 0,
        cancel_reason: cancel_reason.clone(),
    };
    let stream = ReadableStream::builder(source)
        .strategy(CountQueuingStrategy::new(1))
        .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await;
    reader.cancel(None).await.unwrap();
    assert_eq!(*cancel_reason.lock().unwrap(), None);
}

// "ReadableStream: stream.cancel() on a locked stream should reject" (spec §3.3.3)
#[cfg(feature = "send")]
#[tokio::test]
async fn stream_cancel_on_locked_stream_rejects() {
    let stream = ReadableStream::from_vec(vec![1u32]).spawn(tokio::spawn);
    let (_locked, _reader) = stream.get_reader().unwrap();
    assert!(
        stream.cancel(None).await.is_err(),
        "cancel() on a locked stream must return an error per spec §3.3.3"
    );
}

// "ReadableStream: cancel() while pull() is in flight calls source.cancel() with the reason"
//
// pull_with_cancel uses select! so when the cancel signal fires, the pull future
// is dropped (releasing the &mut source borrow) and source is returned to the
// task. The task then calls source.cancel() normally — fully spec-compliant,
// no API change to ReadableSource required.
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_while_pull_in_flight_calls_source_cancel() {
    use std::sync::{Arc, Mutex};

    let cancel_reason: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    struct BlockingSource {
        cancel_reason: Arc<Mutex<Option<String>>>,
    }

    impl ReadableSource<u32> for BlockingSource {
        async fn pull(
            &mut self,
            _controller: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            futures::future::pending::<()>().await;
            Ok(())
        }

        async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
            *self.cancel_reason.lock().unwrap() = reason;
            Ok(())
        }
    }

    let source = BlockingSource {
        cancel_reason: cancel_reason.clone(),
    };
    let stream = ReadableStream::builder(source).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    let cancel_fut = tokio::spawn(async move {
        reader.cancel(Some("in-flight".into())).await
    });

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    cancel_fut.await.unwrap().unwrap();

    assert_eq!(
        cancel_reason.lock().unwrap().as_deref(),
        Some("in-flight"),
        "source.cancel() must be called even when cancel() arrived during a blocking pull()"
    );
}

// WPT: cancel.any.js —
// "cancelling before start finishes should prevent pull() from being called"
#[cfg(feature = "send")]
#[tokio::test]
async fn cancel_before_start_finishes_prevents_pull() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    struct SlowStartSource {
        release: Arc<Notify>,
        pulled: Arc<AtomicBool>,
    }

    impl ReadableSource<u32> for SlowStartSource {
        async fn start(
            &mut self,
            _c: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            // Block start() until the test has issued cancel(), so the cancel is
            // requested while start is still in flight.
            self.release.notified().await;
            Ok(())
        }

        async fn pull(
            &mut self,
            _c: &mut ReadableStreamDefaultController<u32>,
        ) -> StreamResult<()> {
            self.pulled.store(true, Ordering::Release);
            Ok(())
        }
    }

    let release = Arc::new(Notify::new());
    let pulled = Arc::new(AtomicBool::new(false));
    let stream = ReadableStream::builder(SlowStartSource {
        release: release.clone(),
        pulled: pulled.clone(),
    })
    .spawn(tokio::spawn);

    let (_locked, reader) = stream.get_reader().unwrap();

    // Issue cancel() while start() is still blocked; the command queues behind start.
    let cancel_fut = tokio::spawn(async move { reader.cancel(Some("early".into())).await });
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    // Now let start() finish: the queued cancel is processed, closing the stream
    // before the pull-trigger runs, so pull() must never fire.
    release.notify_one();
    cancel_fut.await.unwrap().unwrap();
    tokio::task::yield_now().await;

    assert!(
        !pulled.load(Ordering::Acquire),
        "pull() must not be called when the stream is cancelled before start() finishes"
    );
}
