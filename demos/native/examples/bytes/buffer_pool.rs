//! Buffer Pool Example
//!
//! Demonstrates a simple thread-safe buffer pool for reusing `Vec<u8>` buffers.
//! Useful with BYOB readers or any hot path where repeated allocations would
//! be noticeable. The API returns a `PooledBuf` that returns the buffer to the
//! pool when dropped (unless you consume it with `into_inner()`).

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use tokio::fs::File;
use tokio::io::AsyncReadExt;

/// A simple fixed-size buffer pool.
pub struct BufferPool {
    buffer_size: usize,
    capacity: usize,
    inner: Mutex<Vec<Vec<u8>>>,
}

impl BufferPool {
    /// Create a new pool which keeps up to `capacity` buffers of `buffer_size`.
    pub fn new(buffer_size: usize, capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            buffer_size,
            capacity,
            inner: Mutex::new(Vec::with_capacity(capacity)),
        })
    }

    /// Acquire a pooled buffer. When the returned `PooledBuf` is dropped
    /// the buffer is returned to the pool (if the pool isn't full).
    pub fn acquire(self: &Arc<Self>) -> PooledBuf {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut buf) = guard.pop() {
            // ensure length is the buffer_size so callers get the full capacity as slice
            buf.resize(self.buffer_size, 0);
            PooledBuf {
                pool: Some(Arc::clone(self)),
                buf: Some(buf),
                capacity: self.buffer_size,
            }
        } else {
            // allocate fresh if pool empty
            PooledBuf {
                pool: Some(Arc::clone(self)),
                buf: Some(vec![0u8; self.buffer_size]),
                capacity: self.buffer_size,
            }
        }
    }

    /// Current number of available buffers in pool (for diagnostics).
    pub fn available(&self) -> usize {
        let guard = self.inner.lock().unwrap();
        guard.len()
    }
}

/// A buffer handed out from the pool. When dropped it returns the buffer
/// to the pool automatically (unless `into_inner()` was used).
pub struct PooledBuf {
    pool: Option<Arc<BufferPool>>,
    buf: Option<Vec<u8>>,
    capacity: usize,
}

impl PooledBuf {
    /// Take ownership of the Vec<u8> and prevent it returning to the pool.
    pub fn into_inner(mut self) -> Vec<u8> {
        // prevent returning to pool in Drop
        self.pool = None;
        self.buf.take().unwrap()
    }

    /// Get the buffer length (capacity).
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Deref for PooledBuf {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.buf.as_ref().unwrap().as_slice()
    }
}

impl DerefMut for PooledBuf {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buf.as_mut().unwrap().as_mut_slice()
    }
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        // Try to return buffer to pool; if pool is full, drop buffer.
        if let (Some(pool), Some(mut buf)) = (self.pool.take(), self.buf.take()) {
            // restore length to capacity to keep a uniform pool state
            buf.resize(pool.buffer_size, 0);
            let mut guard = pool.inner.lock().unwrap();
            if guard.len() < pool.capacity {
                guard.push(buf);
            } // else drop
        }
    }
}

/// Demo: read a file using pooled buffers repeatedly to show reuse.
pub async fn run_buffer_pool_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Buffer Pool Example ===");

    // create a small sample file for demo
    let demo_path = "buffer_pool_demo.txt";
    let content = "Hello buffer pool!\n".repeat(512); // ~9KB
    tokio::fs::write(demo_path, &content).await?;

    // create the pool: 16 KB buffers, keep up to 4
    let pool = BufferPool::new(16 * 1024, 4);

    println!("pool available (start): {}", pool.available());

    // open file and repeatedly read into the pooled buffer
    let mut file = File::open(demo_path).await?;
    loop {
        let mut buf = pool.acquire(); // get a PooledBuf
        let n = file.read(&mut *buf).await?;
        if n == 0 {
            break;
        }

        // Process the read bytes (here: print summary)
        println!("read {} bytes (pool available: {})", n, pool.available());

        // optionally shrink/truncate view; we don't change underlying Vec here
        // dropping `buf` returns it to the pool automatically
    }

    println!("pool available (end): {}", pool.available());

    // cleanup
    let _ = tokio::fs::remove_file(demo_path).await;

    println!("✅ Buffer pool demo complete");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_buffer_pool_example().await
}
