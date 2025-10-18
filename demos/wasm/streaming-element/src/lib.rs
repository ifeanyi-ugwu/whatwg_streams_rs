use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{Document, HtmlElement};
use whatwg_streams::local::{WritableSink, WritableStream, WritableStreamDefaultController};

// Our custom sink: appends chunks as <div> children
struct StreamingElementSink {
    parent: HtmlElement,
}

impl WritableSink<String> for StreamingElementSink {
    fn write(
        &mut self,
        chunk: String,
        _controller: &mut WritableStreamDefaultController,
    ) -> impl std::future::Future<Output = whatwg_streams::local::StreamResult<()>> {
        async move {
            let document: Document = web_sys::window().unwrap().document().unwrap();
            let div = document.create_element("div").unwrap();
            div.set_inner_html(&chunk);
            self.parent.append_child(&div).unwrap();
            Ok(())
        }
    }
}

#[wasm_bindgen]
pub fn run_streaming_element_demo(element_id: &str) {
    let document = web_sys::window().unwrap().document().unwrap();
    let el: HtmlElement = document
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    // Create the writable stream
    let sink = StreamingElementSink { parent: el };
    let writable = WritableStream::builder(sink).spawn(spawn_local);

    // Example chunks to stream
    let chunks = vec![
        "Hello from Rust + WASM!".to_string(),
        "Streaming into a custom element.".to_string(),
        "Each chunk is appended as a <div>.".to_string(),
    ];

    // Write all chunks asynchronously
    spawn_local(async move {
        let (_, writer) = writable.get_writer().unwrap();
        for chunk in chunks {
            writer.write(chunk).await.unwrap();
        }
        writer.close().await.unwrap();
    });
}
