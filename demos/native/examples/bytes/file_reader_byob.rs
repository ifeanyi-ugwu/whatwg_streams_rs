//! Reading a file with a BYOB reader and caller-controlled buffer sizes.
//!
//! A byte `ReadableStream` over a file is read through a BYOB reader: each `read` fills a
//! buffer the caller supplies, so the caller decides how large each read is (here cycling
//! 4 KB / 8 KB / 16 KB) and owns the allocation. The source reads straight into that
//! buffer, with no intermediate per-chunk `Vec`.
//!
//! Run with: `cargo run --example file_reader_byob`

use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use whatwg_streams::{
    ReadableByteSource, ReadableByteStreamController, ReadableStream, StreamResult,
};

/// Byte source that reads a file into each provided BYOB buffer.
pub struct FileByobSource {
    file: Option<File>,
    path: PathBuf,
}

impl FileByobSource {
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            file: None,
            path: path.into(),
        }
    }
}

impl ReadableByteSource for FileByobSource {
    async fn start(&mut self, _c: &mut ReadableByteStreamController) -> StreamResult<()> {
        self.file = Some(File::open(&self.path).await?);
        Ok(())
    }

    async fn pull(
        &mut self,
        controller: &mut ReadableByteStreamController,
        buffer: &mut [u8],
    ) -> StreamResult<usize> {
        let file = self.file.as_mut().expect("opened in start");
        let n = file.read(buffer).await?;
        if n == 0 {
            controller.close()?;
        }
        Ok(n)
    }
}

#[tokio::main]
async fn main() {
    let path = "byob_sample.txt";
    tokio::fs::write(path, "A".repeat(64 * 1024))
        .await
        .expect("write sample file");

    let stream = ReadableStream::builder_bytes(FileByobSource::new(path)).spawn(tokio::spawn);
    let (_lock, reader) = stream.get_byob_reader().expect("a fresh stream is unlocked");

    // The caller picks each read's buffer size; BYOB fills exactly that buffer.
    let sizes = [4096usize, 8192, 16384];
    let mut total = 0u64;
    let mut reads = 0;

    loop {
        let size = sizes[reads % sizes.len()];
        let mut buffer = vec![0u8; size];
        let n = reader.read(&mut buffer).await.expect("read should not error");
        if n == 0 {
            break;
        }
        reads += 1;
        total += n as u64;
        println!("read {n} bytes into a {size}-byte buffer (running total {total})");
    }

    println!("done: {total} bytes in {reads} reads");
    let _ = tokio::fs::remove_file(path).await;
}
