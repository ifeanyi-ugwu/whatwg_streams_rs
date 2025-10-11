//! CSV Processor Example
//!
//! Demonstrates a transform that filters CSV rows by column value:
//! 1. Reads CSV rows as strings
//! 2. Splits by commas
//! 3. Filters rows where column `filter_column` matches `filter_value`
//! 4. Writes results to a new CSV file

use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use whatwg_streams::dlc::ideation::d::{
    CountQueuingStrategy, StreamResult,
    readable_sampling_b::{ReadableSource, ReadableStream, ReadableStreamDefaultController},
    transform::{TransformStream, TransformStreamDefaultController, Transformer},
    writable_new::{WritableSink, WritableStream, WritableStreamDefaultController},
};

/// Mock CSV data source
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
            let row = self.rows[self.index].clone();
            self.index += 1;
            controller.enqueue(row)?;
        } else {
            controller.close()?;
        }
        Ok(())
    }
}

/// Transform: filter CSV rows by column value
pub struct CsvRowFilter {
    pub filter_column: usize,
    pub filter_value: String,
}

impl Transformer<String, String> for CsvRowFilter {
    fn transform(
        &mut self,
        chunk: String,
        controller: &mut TransformStreamDefaultController<String>,
    ) -> impl futures::Future<Output = StreamResult<()>> + Send {
        let col = self.filter_column;
        let val = self.filter_value.clone();

        futures::future::ready({
            let cols: Vec<&str> = chunk.split(',').collect();
            if cols.get(col).map_or(false, |&c| c == val) {
                controller.enqueue(chunk);
            }
            Ok(())
        })
    }
}

/// Writable sink: writes CSV rows to file
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
}

/// Example pipeline: read -> filter -> write
pub async fn run_csv_processor_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== CSV Processor Example ===");

    let rows = vec![
        "id,name,role",
        "1,Alice,admin",
        "2,Bob,user",
        "3,Carol,admin",
        "4,Dan,user",
    ];

    let source = ReadableStream::new_default(CsvSource::new(rows), None);
    let filter = TransformStream::new(CsvRowFilter {
        filter_column: 2,
        filter_value: "admin".into(),
    });
    let writer = WritableStream::new_with_spawn(
        CsvFileWriter::new("filtered_admins.csv"),
        Box::new(CountQueuingStrategy::new(2)),
        |fut| {
            tokio::spawn(fut);
        },
    );

    source
        .pipe_through(filter, None)
        .pipe_to(&writer, None)
        .await?;
    println!("✅ Filtered CSV written to filtered_admins.csv");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run_csv_processor_example().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::fs;

    #[tokio::test]
    async fn test_csv_filter_pipeline() {
        let rows = vec![
            "id,name,role",
            "1,Alice,admin",
            "2,Bob,user",
            "3,Carol,admin",
            "4,Dan,user",
        ];

        let readable = ReadableStream::new_default(CsvSource::new(rows), None);
        let filter = TransformStream::new(CsvRowFilter {
            filter_column: 2,
            filter_value: "admin".into(),
        });
        let output_path = "test_filtered_admins.csv";
        let writable = WritableStream::new_with_spawn(
            CsvFileWriter::new(output_path),
            Box::new(CountQueuingStrategy::new(2)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        // Run pipeline
        readable
            .pipe_through(filter, None)
            .pipe_to(&writable, None)
            .await
            .expect("CSV pipeline should succeed");

        // Verify output file
        let content = fs::read_to_string(output_path).await.unwrap();
        let lines: Vec<&str> = content.trim().split('\n').collect();

        // Should keep header + 2 admin rows
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "id,name,role");
        assert_eq!(lines[1], "1,Alice,admin");
        assert_eq!(lines[2], "3,Carol,admin");

        // Clean up
        let _ = fs::remove_file(output_path).await;
    }

    #[tokio::test]
    async fn test_empty_csv_source() {
        let readable = ReadableStream::new_default(CsvSource::new(vec![]), None);
        let filter = TransformStream::new(CsvRowFilter {
            filter_column: 2,
            filter_value: "admin".into(),
        });
        let output_path = "test_empty_csv.csv";
        let writable = WritableStream::new_with_spawn(
            CsvFileWriter::new(output_path),
            Box::new(CountQueuingStrategy::new(1)),
            |fut| {
                tokio::spawn(fut);
            },
        );

        readable
            .pipe_through(filter, None)
            .pipe_to(&writable, None)
            .await
            .expect("Empty CSV pipeline should complete");

        let content = fs::read_to_string(output_path).await.unwrap();
        assert!(content.is_empty());

        // Clean up
        let _ = fs::remove_file(output_path).await;
    }
}
