//! Reusing buffers across BYOB reads with a simple pool.
//!
//! A BYOB reader fills a caller-provided buffer on each read, which makes it a natural
//! fit for buffer reuse: rather than allocating a fresh `Vec` per read, each read borrows
//! a buffer from a pool and returns it on drop. This streams a file through a byte
//! `ReadableStream`, reading into pooled buffers; the "buffers in pool" line shows a
//! returned buffer being handed back out instead of a new allocation.
//!
//! Run with: `cargo run --example buffer_pool`

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use whatwg_streams::{ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult};

/// A fixed-size buffer pool that hands out reusable `Vec<u8>` buffers.
pub struct BufferPool {
    buffer_size: usize,
    capacity: usize,
    free: Mutex<Vec<Vec<u8>>>,
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
    pub fn acquire(self: &Arc<Self>) -> PooledBuf {
        let buf = self
            .free
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| vec![0u8; self.buffer_size]);
        PooledBuf {
            pool: Arc::clone(self),
            buf: Some(buf),
        }
    }

    /// Number of buffers currently available for reuse.
    pub fn available(&self) -> usize {
        self.free.lock().unwrap().len()
    }
}

/// A buffer borrowed from the pool; returns itself on drop unless the pool is full.
pub struct PooledBuf {
    pool: Arc<BufferPool>,
    buf: Option<Vec<u8>>,
}

impl Deref for PooledBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.buf.as_ref().unwrap()
    }
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.buf.as_mut().unwrap()
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        if let Some(mut buf) = self.buf.take() {
            buf.resize(self.pool.buffer_size, 0);
            let mut free = self.pool.free.lock().unwrap();
            if free.len() < self.pool.capacity {
                free.push(buf);
            }
        }
    }
}

/// Byte source that fills each BYOB buffer from a file.
pub struct FileByteSource {
    file: File,
}

impl ReadableByteSource for FileByteSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        let n = self.file.read(buffer).await?;
        if n == 0 {
            controller.close()?;
        }
        Ok(n)
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
        println!("buffers in pool: {}", pool.available()); // 0 first time, 1+ once reused
        let mut buf = pool.acquire();
        let n = reader.read(&mut buf).await.expect("read should not error");
        if n == 0 {
            break;
        }
        total += n;
        // `buf` returns to the pool here, on drop.
    }
    println!("read {total} bytes total");

    let _ = tokio::fs::remove_file(demo_path).await;
}
