//! Streaming files into a `.tar.gz`, written incrementally.
//!
//! Each input file is appended to a `tar::Builder`; after every file the newly produced
//! tar bytes are drained (`get_mut` + `mem::take`) and written to the stream as one
//! chunk, so the full archive is never held in memory — only the current file's tar
//! block. The writable sink gzips each chunk straight to disk.
//!
//! Run with: `cargo run --example archive_creator`

use flate2::{Compression, write::GzEncoder};
use std::fs::File as StdFile;
use std::io::Write;
use tar::{Builder, Header};
use whatwg_streams::{
    StreamResult, WritableSink, WritableStream, WritableStreamDefaultController,
};

/// Gzips each written chunk straight into a file.
pub struct TarGzWriter {
    file_path: String,
    encoder: Option<GzEncoder<StdFile>>,
}

impl TarGzWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            encoder: None,
        }
    }
}

impl WritableSink<Vec<u8>> for TarGzWriter {
    async fn start(&mut self, _c: &mut WritableStreamDefaultController) -> StreamResult<()> {
        let file = StdFile::create(&self.file_path)?;
        self.encoder = Some(GzEncoder::new(file, Compression::default()));
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: Vec<u8>,
        _c: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(encoder) = &mut self.encoder {
            encoder.write_all(&chunk)?;
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(encoder) = self.encoder.take() {
            encoder.finish()?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let input_files: Vec<(&str, &[u8])> = vec![
        ("file1.txt", b"Hello, world! This is file 1."),
        ("file2.txt", b"Another file content for the archive."),
        ("file3.txt", b"Final file data here."),
    ];

    let archive_file = "archive_output.tar.gz";
    let sink = WritableStream::builder(TarGzWriter::new(archive_file)).spawn(tokio::spawn);
    let (_lock, writer) = sink.get_writer().expect("a fresh stream is unlocked");

    // The builder writes into an owned Vec; after each file, drain what it produced and
    // hand that chunk to the stream, so only one file's tar block is buffered at a time.
    let mut builder = Builder::new(Vec::new());
    for (name, content) in &input_files {
        let mut header = Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, name, &content[..])
            .expect("append to tar");

        let produced = std::mem::take(builder.get_mut());
        writer.write(produced).await.expect("write tar chunk");
        println!("streamed {name} ({} bytes) into the archive", content.len());
    }

    // `into_inner` finalizes the archive (writes the trailing zero blocks).
    let trailer = builder.into_inner().expect("finish tar");
    writer.write(trailer).await.expect("write tar trailer");
    writer.close().await.expect("close archive");

    let size = tokio::fs::metadata(archive_file)
        .await
        .expect("stat archive")
        .len();
    println!("created {archive_file} ({size} bytes)");
}
