// WPT: streams/readable-streams/general.any.js

use whatwg_streams::{
    CountQueuingStrategy, ReadableSource, ReadableStream, ReadableStreamDefaultController,
    StreamError, StreamResult,
};

struct FailingStartSource;

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

struct FailingPullSource;

impl ReadableSource<u32> for FailingPullSource {
    async fn pull(
        &mut self,
        _controller: &mut ReadableStreamDefaultController<u32>,
    ) -> StreamResult<()> {
        Err(StreamError::from("pull rejected"))
    }
}

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
    let _ = reader.read().await;
    reader.closed().await.unwrap();
}

// "ReadableStream: closed promise rejects when the stream errors"
#[cfg(feature = "send")]
#[tokio::test]
async fn closed_rejects_when_stream_errored() {
    let stream = ReadableStream::builder(FailingPullSource).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let _ = reader.read().await;
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

// "ReadableStream: desired_size reflects HWM minus queue depth"
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
    let _ = reader.read().await;
    tokio::task::yield_now().await;

    assert_eq!(*captured.lock().unwrap(), Some(4));
}
