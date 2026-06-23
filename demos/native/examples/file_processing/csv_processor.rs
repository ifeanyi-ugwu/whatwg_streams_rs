//! Filtering CSV rows by a column value, as a `TransformStream`.
//!
//! Rows arrive as raw lines; the transform splits each on commas and forwards only the
//! rows whose chosen column matches a target value (the header row matches nothing, so
//! it is dropped). Surviving rows are written to a new CSV file.
//!
//! Run with: `cargo run --example csv_processor`

use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult, TransformStream,
    TransformStreamDefaultController, Transformer, WritableSink, WritableStream,
    WritableStreamDefaultController,
};

/// Yields CSV rows from memory, then closes.
pub struct CsvSource {
    rows: Vec<String>,
    index: usize,
}

impl CsvSource {
    pub fn new(rows: Vec<&str>) -> Self {
        Self {
            rows: rows.into_iter().map(String::from).collect(),
            index: 0,
        }
    }
}

impl ReadableSource<String> for CsvSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if self.index < self.rows.len() {
            controller.enqueue(self.rows[self.index].clone())?;
            self.index += 1;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Forwards only rows whose `column` equals `value`.
pub struct CsvRowFilter {
    pub column: usize,
    pub value: String,
}

impl Transformer<String, String> for CsvRowFilter {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> StreamResult<()> {
        if chunk.split(',').nth(self.column) == Some(self.value.as_str()) {
            controller.enqueue(chunk)?;
        }
        Ok(())
    }
}

/// Appends each row as a line, opening the file on the first write.
pub struct CsvFileWriter {
    file_path: String,
    file: Option<File>,
}

impl CsvFileWriter {
    pub fn new(path: &str) -> Self {
        Self {
            file_path: path.to_string(),
            file: None,
        }
    }
}

impl WritableSink<String> for CsvFileWriter {
    async fn start(&mut self, _c: &mut WritableStreamDefaultController) -> StreamResult<()> {
        self.file = Some(File::create(&self.file_path).await?);
        Ok(())
    }

    async fn write(
        &mut self,
        chunk: String,
        _c: &mut WritableStreamDefaultController,
    ) -> StreamResult<()> {
        if let Some(file) = &mut self.file {
            file.write_all(chunk.as_bytes()).await?;
            file.write_all(b"\n").await?;
        }
        Ok(())
    }

    async fn close(mut self) -> StreamResult<()> {
        if let Some(file) = self.file.take() {
            file.sync_all().await?;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let rows = vec![
        "id,name,role",
        "1,Alice,admin",
        "2,Bob,user",
        "3,Carol,admin",
        "4,Dan,user",
    ];

    let output_path = "filtered_admins.csv";
    let source = ReadableStream::builder(CsvSource::new(rows)).spawn(tokio::spawn);
    let filter = TransformStream::builder(CsvRowFilter {
        column: 2,
        value: "admin".into(),
    })
    .spawn(tokio::spawn);
    let sink = WritableStream::builder(CsvFileWriter::new(output_path)).spawn(tokio::spawn);

    let admins = source.pipe_through(filter, None).spawn(tokio::spawn);
    admins
        .pipe_to(&sink, None)
        .await
        .expect("pipe should complete");

    let content = tokio::fs::read_to_string(output_path)
        .await
        .expect("read back output");
    println!("wrote admin rows to {output_path}:");
    print!("{content}");
}
