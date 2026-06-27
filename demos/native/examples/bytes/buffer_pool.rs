//! Zero-copy buffer reuse with `read_owned` and a simple pool.
//!
//! `read_owned` transfers an owned buffer into the stream; the source fills it
//! directly (via `byob_request` / `respond`) and ownership returns to the caller,
//! so the file's bytes land straight in the caller's buffer with no queue copy.
//! Because the buffer comes back, it can be reused: each read borrows a buffer
//! from a pool and returns it afterwards, so steady state allocates nothing. The
//! "buffers available for reuse" line shows a returned buffer being handed back
//! out instead of a new allocation.
//!
//! The owned handoff is why this is explicit acquire/release rather than a
//! Drop-guard: the buffer leaves the caller for the duration of the read, so it
//! cannot return itself on scope exit — it comes back as a value from `read_owned`.
//!
//! Run with: `cargo run --example buffer_pool`

use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use whatwg_streams::{
    BytesMut, ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult,
};

/// A fixed-size pool of reusable buffers.
pub struct BufferPool {
    buffer_size: usize,
    capacity: usize,
    free: Mutex<Vec<BytesMut>>,
}

impl BufferPool {
    /// Keeps up to `capacity` buffers of `buffer_size` bytes.
    pub fn new(buffer_size: usize, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            buffer_size,
            capacity,
            free: Mutex::new(Vec::with_capacity(capacity)),
        })
    }

    /// Borrow a buffer, reusing a returned one when available.
    pub fn acquire(&self) -> BytesMut {
        self.free
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| BytesMut::zeroed(self.buffer_size))
    }

    /// Return a buffer for reuse (dropped if the pool is already full). `read_owned`
    /// fills `[..n]` and hands the whole buffer back, so it is reset to full length
    /// for the next read.
    pub fn release(&self, mut buf: BytesMut) {
        let mut free = self.free.lock().unwrap();
        if free.len() < self.capacity {
            buf.resize(self.buffer_size, 0);
            free.push(buf);
        }
    }

    /// Number of buffers currently available for reuse.
    pub fn available(&self) -> usize {
        self.free.lock().unwrap().len()
    }
}

/// Byte source that fills the reader's transferred buffer straight from a file.
pub struct FileByteSource {
    file: File,
}

impl ReadableByteSource for FileByteSource {
    async fn pull(&mut self, controller: &mut ReadableByteStreamController) -> StreamResult<()> {
        if let Some(mut req) = controller.byob_request() {
            // Zero-copy: read the file straight into the reader's pooled buffer.
            let n = self.file.read(&mut req).await?;
            if n == 0 {
                controller.close()?;
            }
            req.respond(n)?;
        } else {
            // No BYOB read waiting (e.g. a default reader): enqueue our own buffer.
            let mut buf = vec![0u8; 8192];
            let n = self.file.read(&mut buf).await?;
            if n == 0 {
                controller.close()?;
            } else {
                buf.truncate(n);
                controller.enqueue(buf)?;
            }
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let demo_path = "buffer_pool_demo.txt";
    tokio::fs::write(demo_path, "Hello buffer pool!\n".repeat(512))
        .await
        .expect("write demo file");

    // 4 KB buffers so the ~9.5 KB file takes several reads and buffer reuse is visible.
    let pool = BufferPool::new(4 * 1024, 4);
    let file = File::open(demo_path).await.expect("open demo file");
    let stream = ReadableStream::builder_bytes(FileByteSource { file }).spawn(tokio::spawn);
    let (_lock, reader) = stream.get_byob_reader().expect("a fresh stream is unlocked");

    let mut total = 0;
    loop {
        println!("buffers available for reuse: {}", pool.available()); // 0 first, 1+ once reused
        let buf = pool.acquire();
        let (buf, n) = reader.read_owned(buf).await.expect("read should not error");
        if n == 0 {
            pool.release(buf);
            break;
        }
        total += n;
        // Process &buf[..n] here — it is done before the next read, so the buffer
        // goes straight back to the pool.
        pool.release(buf);
    }
    println!("read {total} bytes total");

    let _ = tokio::fs::remove_file(demo_path).await;
}
