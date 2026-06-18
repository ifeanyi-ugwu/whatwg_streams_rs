// WPT: streams/readable-streams/byte-source.any.js

use whatwg_streams::{
    ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult,
};

struct ChunkedByteSource {
    chunks: Vec<Vec<u8>>,
    index: std::sync::Arc<std::sync::Mutex<usize>>,
    cancel_reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

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

struct FailingByteStart;

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

struct FailingBytePull;

impl ReadableByteSource for FailingBytePull {
    async fn pull(
        &mut self,
        _controller: &mut ReadableByteStreamController,
        _buffer: &mut [u8],
    ) -> StreamResult<usize> {
        Err("pull failed".into())
    }
}

// ── Default reader ─────────────────────────────────────────────────────────────

// "ReadableByteStream: default reader reads all chunks in order"
#[cfg(feature = "send")]
#[tokio::test]
async fn default_reader_reads_all_chunks() {
    let source = ChunkedByteSource {
        chunks: vec![b"hello".to_vec(), b" world".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    let mut all = Vec::new();
    while let Some(chunk) = reader.read().await.unwrap() {
        all.extend_from_slice(&chunk);
    }
    assert_eq!(all, b"hello world");
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

// "ReadableByteStream: if start() rejects, reads error"
#[cfg(feature = "send")]
#[tokio::test]
async fn start_rejection_errors_reads() {
    let stream = ReadableStream::builder_bytes(FailingByteStart).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// "ReadableByteStream: if pull() rejects, reads error"
#[cfg(feature = "send")]
#[tokio::test]
async fn pull_rejection_errors_reads() {
    let stream = ReadableStream::builder_bytes(FailingBytePull).spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();
    assert!(reader.read().await.is_err());
}

// ── Locking ────────────────────────────────────────────────────────────────────

// "ReadableByteStream: get_byob_reader() fails if already locked"
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
    assert!(stream.get_byob_reader().is_err());
}

// "ReadableByteStream: get_reader() fails if BYOB reader already holds the lock"
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
    assert!(stream.get_reader().is_err());
}

// ── BYOB reader ────────────────────────────────────────────────────────────────

// "BYOB reader: reads into caller-supplied buffer"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_reads_into_buffer() {
    let source = ChunkedByteSource {
        chunks: vec![b"hello".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();
    let mut buf = [0u8; 16];
    let n = byob.read(&mut buf).await.unwrap();
    assert!(n > 0);
    assert_eq!(&buf[..n], b"hello");
}

// "BYOB reader: returns 0 after EOF"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_returns_zero_at_eof() {
    let source = ChunkedByteSource {
        chunks: vec![b"hi".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();
    let mut buf = [0u8; 16];
    let _ = byob.read(&mut buf).await.unwrap();
    assert_eq!(byob.read(&mut buf).await.unwrap(), 0);
}

// "BYOB reader: closed promise resolves at EOF"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_closed_resolves_at_eof() {
    let source = ChunkedByteSource {
        chunks: vec![],
        index: Default::default(),
        cancel_reason: Default::default(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();
    let mut buf = [0u8; 16];
    byob.read(&mut buf).await.unwrap();
    byob.closed().await.unwrap();
}

// "BYOB reader: release_lock() allows acquiring a new reader"
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
    let (_locked, reader) = stream.get_reader().unwrap();
    assert_eq!(reader.read().await.unwrap(), Some(b"abc".to_vec()));
}

// "BYOB reader: cancel() calls source cancel() with the given reason"
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_cancel_calls_source_cancel() {
    let cancel_reason = std::sync::Arc::new(std::sync::Mutex::new(None));
    let source = ChunkedByteSource {
        chunks: vec![b"data".to_vec()],
        index: Default::default(),
        cancel_reason: cancel_reason.clone(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();
    byob.cancel(Some("done".into())).await.unwrap();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(cancel_reason.lock().unwrap().as_deref(), Some("done"));
}

// ── Trait integrations ─────────────────────────────────────────────────────────

// "ReadableByteStream: AsyncRead trait reads bytes to end"
#[cfg(feature = "send")]
#[tokio::test]
async fn async_read_trait_reads_to_end() {
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

// ── WPT: readable-byte-streams/general.any.js (translatable behaviours) ───────

// "enqueue(), getReader(), then read(view) with smaller views"
// A BYOB read with a buffer smaller than the queued data takes only what fits;
// the remainder is served by the next read.
#[cfg(feature = "send")]
#[tokio::test]
async fn byob_partial_read_serves_remainder() {
    let source = ChunkedByteSource {
        chunks: vec![b"hello".to_vec()], // 5 bytes enqueued in one pull
        index: Default::default(),
        cancel_reason: Default::default(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (_locked, byob) = stream.get_byob_reader().unwrap();

    let mut buf = [0u8; 3];
    let n1 = byob.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n1], b"hel", "first read fills the 3-byte view");

    let mut buf2 = [0u8; 3];
    let n2 = byob.read(&mut buf2).await.unwrap();
    assert_eq!(&buf2[..n2], b"lo", "the remainder is served on the next read");
}

// "Throw on enqueue() after close()" + "Throw if close()-ed more than once"
// The byte controller's close()/enqueue() return Result; both must reject once the
// stream is closed.
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_controller_guards_after_close() {
    use std::sync::{Arc, Mutex};

    let enqueue_err = Arc::new(Mutex::new(None));
    let close_again_err = Arc::new(Mutex::new(None));

    struct GuardSource {
        enqueue_err: Arc<Mutex<Option<bool>>>,
        close_again_err: Arc<Mutex<Option<bool>>>,
    }
    impl ReadableByteSource for GuardSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableByteStreamController,
            _buffer: &mut [u8],
        ) -> StreamResult<usize> {
            controller.close()?;
            *self.enqueue_err.lock().unwrap() = Some(controller.enqueue(b"x".to_vec()).is_err());
            *self.close_again_err.lock().unwrap() = Some(controller.close().is_err());
            Ok(0)
        }
    }

    let stream = ReadableStream::builder_bytes(GuardSource {
        enqueue_err: enqueue_err.clone(),
        close_again_err: close_again_err.clone(),
    })
    .spawn(tokio::spawn);
    let (_locked, reader) = stream.get_reader().unwrap();

    assert_eq!(reader.read().await.unwrap(), None, "stream closes");
    assert_eq!(*enqueue_err.lock().unwrap(), Some(true), "enqueue() after close() must error");
    assert_eq!(*close_again_err.lock().unwrap(), Some(true), "close() twice must error");
}

// ── Teeing ───────────────────────────────────────────────────────────────────
//
// tee() on a byte stream rides the generic tee path: the branches are default
// streams, not byte streams with BYOB readers. The branch-side behaviours
// (cancel-one, close/error propagation) are the generic tee's, covered in
// readable/tee.rs. What is byte-distinct, and pinned here, is that the source is
// consumed through the byte task's read/cancel handlers — a different code path
// from a default source feeding the same coordinator.

// "ReadableStream teeing with byte source: both branches read the source to the end"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_tee_both_branches_receive_all_bytes() {
    let source = ChunkedByteSource {
        chunks: vec![b"hello".to_vec(), b" world".to_vec()],
        index: Default::default(),
        cancel_reason: Default::default(),
    };
    let stream = ReadableStream::builder_bytes(source).spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_l1, r1) = branch1.get_reader().unwrap();
    let (_l2, r2) = branch2.get_reader().unwrap();

    let mut b1 = Vec::new();
    while let Some(c) = r1.read().await.unwrap() {
        b1.extend_from_slice(&c);
    }
    let mut b2 = Vec::new();
    while let Some(c) = r2.read().await.unwrap() {
        b2.extend_from_slice(&c);
    }
    assert_eq!(b1, b"hello world");
    assert_eq!(b2, b"hello world");
}

// "ReadableStream teeing with byte source: canceling both branches cancels the source"
#[cfg(feature = "send")]
#[tokio::test]
async fn byte_tee_cancelling_both_branches_cancels_source() {
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    struct CancelCountingByteSource {
        cancels: Arc<AtomicU32>,
    }
    impl ReadableByteSource for CancelCountingByteSource {
        async fn pull(
            &mut self,
            controller: &mut ReadableByteStreamController,
            _buffer: &mut [u8],
        ) -> StreamResult<usize> {
            controller.enqueue(b"data".to_vec())?;
            Ok(0)
        }
        async fn cancel(&mut self, _reason: Option<String>) -> StreamResult<()> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    let cancels = Arc::new(AtomicU32::new(0));
    let stream = ReadableStream::builder_bytes(CancelCountingByteSource {
        cancels: cancels.clone(),
    })
    .spawn(tokio::spawn);
    let (branch1, branch2) = stream.tee().spawn(tokio::spawn).unwrap();
    let (_l1, r1) = branch1.get_reader().unwrap();
    let (_l2, r2) = branch2.get_reader().unwrap();

    r1.cancel(None).await.unwrap();
    r2.cancel(None).await.unwrap();

    assert_eq!(
        cancels.load(Ordering::SeqCst),
        1,
        "byte source cancel() must fire exactly once when both tee branches cancel"
    );
}
