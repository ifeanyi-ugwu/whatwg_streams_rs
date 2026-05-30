//! Integration tests for ReadableStream byte streams (BYOB reader),
//! ported from:
//! WPT: streams/readable-streams/byte-source.any.js
//! https://github.com/web-platform-tests/wpt/tree/master/streams/readable-streams

use whatwg_streams::{
    ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Source that yields a fixed sequence of byte chunks and then signals EOF.
#[cfg(feature = "send")]
struct ChunkedByteSource {
    chunks: Vec<Vec<u8>>,
    index: std::sync::Arc<std::sync::Mutex<usize>>,
    cancel_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

#[cfg(feature = "send")]
impl ReadableByteSource for ChunkedByteSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        _buffer: &mut [u8],
    ) -> StreamResult<usize> {
        let idx = {
            let mut i = self.index.lock().unwrap();
            let v = *i;
            *i += 1;
            v
        };

        if idx < self.chunks.len() {
            controller.enqueue(self.chunks[idx].clone())?;
        } else {
            controller.close()?;
        }
        Ok(0)
    }

    async fn cancel(&mut self, reason: Option<String>) -> StreamResult<()> {
        *self.cancel_reason.lock().unwrap() = reason;
        Ok(())
    }
}

/// Source whose start() fails immediately.
#[cfg(feature = "send")]
struct FailingByteStart;

#[cfg(feature = "send")]
impl ReadableByteSource for FailingByteStart {
    async fn start(&mut self, _controller: &mut ReadableByteStreamController) -> StreamResult<()> {
        Err("start failed".into())
    }

    async fn pull(
        &mut self,
        _controller: &mut ReadableByteStreamController,
        _buffer: &mut [u8],
    ) -> StreamResult<usize> {
        Ok(0)
    }
}

/// Source whose pull() always fails.
#[cfg(feature = "send")]
struct FailingBytePull;

#[cfg(feature = "send")]
impl ReadableByteSource for FailingBytePull {
    async fn pull(
        &mut self,
        _controller: &mut ReadableByteStreamController,
        _buffer: &mut [u8],
    ) -> StreamResult<usize> {
        Err("pull failed".into())
    }
}

// ── WPT: readable-streams/byte-source.any.js ─────────────────────────────────

// "ReadableByteStream: default reader reads all chunks in order"
#[cfg(feature = "send")]
#[tokio::test]
async fn default_reader_reads_all_chunks() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
    let source = ChunkedByteSource {
        chunks: vec![b"hello".to_vec(), b" world".to_vec()],
        index: Default::default(),
        cancel_reason: cancel_reason.clone(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    let mut all_bytes = Vec::new();
    while let Some(chunk) = reader.read().await.unwrap() {
        all_bytes.extend_from_slice(&chunk);
    }

    assert_eq!(all_bytes, b"hello world");
}

// "ReadableByteStream: default reader closes after EOF"
#[cfg(feature = "send")]
#[tokio::test]
async fn default_reader_closes_at_eof() {
    let source = ChunkedByteSource {
        chunks: vec![b"data".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    assert_eq!(reader.read().await.unwrap(), Some(b"data".to_vec()));
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableByteStream: reading from an empty source gives None immediately"
#[cfg(feature = "send")]
#[tokio::test]
async fn empty_source_gives_none() {
    let source = ChunkedByteSource {
        chunks: vec![],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), None);
}

// "ReadableByteStream: get_byob_reader() fails if stream is already locked"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_byob_reader_fails_when_locked() {
    let source = ChunkedByteSource {
        chunks: vec![b"x".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, _reader) = stream.get_reader().unwrap();
    assert!(
        stream.get_byob_reader().is_err(),
        "get_byob_reader() should fail when stream is locked"
    );
}

// "ReadableByteStream: get_reader() fails if stream already has a BYOB reader"
#[cfg(feature = "send")]
#[tokio::test]
async fn get_default_reader_fails_when_byob_locked() {
    let source = ChunkedByteSource {
        chunks: vec![b"x".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, _byob) = stream.get_byob_reader().unwrap();
    assert!(
        stream.get_reader().is_err(),
        "get_reader() should fail when BYOB reader already holds the lock"
    );
}

// ── BYOB reader ───────────────────────────────────────────────────────────────

// "ReadableByteStream BYOB reader: reads into caller-supplied buffer"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_reader_reads_into_supplied_buffer() {
    let source = ChunkedByteSource {
        chunks: vec![b"hello".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();

    let mut buf = [0u8; 16];
    let n = byob.read(&mut buf).await.unwrap();
    assert!(n > 0, "BYOB read should return bytes");
    assert_eq!(&buf[..n], b"hello");
}

// "ReadableByteStream BYOB reader: returns 0 after EOF"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_reader_returns_zero_at_eof() {
    let source = ChunkedByteSource {
        chunks: vec![b"hi".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();

    let mut buf = [0u8; 16];
    let _ = byob.read(&mut buf).await.unwrap(); // consume data
    let n = byob.read(&mut buf).await.unwrap();
    assert_eq!(n, 0, "BYOB read after EOF should return 0");
}

// "ReadableByteStream BYOB reader: closed promise resolves after EOF"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_reader_closed_resolves_at_eof() {
    let source = ChunkedByteSource {
        chunks: vec![],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();

    let mut buf = [0u8; 16];
    byob.read(&mut buf).await.unwrap(); // triggers EOF
    byob.closed().await.unwrap();
}

// "ReadableByteStream BYOB reader: release_lock() allows a new reader"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_release_lock_allows_new_reader() {
    let source = ChunkedByteSource {
        chunks: vec![b"abc".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();
    let stream = byob.release_lock();

    // Should now be able to get a default reader
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(b"abc".to_vec()));
}

// "ReadableByteStream: if start() rejects, reads error"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_stream_start_rejection_errors_reads() {
    let stream = ReadableStream::builder_bytes(FailingByteStart).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    // Start fails, so reads should return an error
    let result = reader.read().await;
    assert!(result.is_err(), "reads should error when start() rejects");
}

// "ReadableByteStream: cancel() returns Ok and calls source cancel()"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_stream_cancel_calls_source_cancel() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
    let source = ChunkedByteSource {
        chunks: vec![b"data".to_vec()],
        index: Default::default(),
        cancel_reason: cancel_reason.clone(),
    };

    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();
    byob.cancel(Some("done".into())).await.unwrap();

    // Give the cancel time to propagate
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    assert_eq!(
        cancel_reason.lock().unwrap().as_deref(),
        Some("done"),
        "source.cancel() should have been called with the reason"
    );
}

// "ReadableByteStream: if pull() rejects, reads return an error"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_stream_pull_rejection_errors_reads() {
    let stream = ReadableStream::builder_bytes(FailingBytePull).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(
        reader.read().await.is_err(),
        "reads should error when pull() rejects"
    );
}

// "ReadableByteStream: AsyncRead trait reads bytes"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_read_trait_reads_bytes() {
    use futures::AsyncReadExt;

    let source = ChunkedByteSource {
        chunks: vec![b"async".to_vec(), b" read".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };

    let mut stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let mut out = Vec::new();
    stream.read_to_end(&mut out).await.unwrap();
    assert_eq!(out, b"async read");
}
