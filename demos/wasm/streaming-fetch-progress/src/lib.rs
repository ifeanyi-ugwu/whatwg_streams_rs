use js_sys::{Function, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlElement;
use whatwg_streams::local::{ReadableSource, ReadableStream, ReadableStreamDefaultController};

struct FetchByteSource {
    reader: Object,
}

impl ReadableSource<Vec<u8>> for FetchByteSource {
    fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> impl std::future::Future<Output = whatwg_streams::local::StreamResult<()>> {
        let reader = self.reader.clone();
        async move {
            // Get the 'read' method from the reader
            let read_fn: Function = Reflect::get(&reader, &JsValue::from_str("read"))
                .unwrap()
                .dyn_into()
                .unwrap();

            // Call reader.read()
            let promise_js: JsValue = read_fn.call0(&reader).unwrap();
            let promise: js_sys::Promise = promise_js.dyn_into().unwrap();
            let chunk = wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();

            // Get the value
            let value = Reflect::get(&chunk, &JsValue::from_str("value")).unwrap();
            if value.is_null() {
                Ok(())
            } else {
                let array = Uint8Array::new(&value);
                let _ = controller.enqueue(array.to_vec());
                Ok(())
            }
        }
    }
}

#[wasm_bindgen]
pub fn run_fetch_progress_demo(url: String, progress_id: String) {
    let document = web_sys::window().unwrap().document().unwrap();
    let progress_el: HtmlElement = document
        .get_element_by_id(&progress_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    spawn_local(async move {
        // Fetch
        let resp =
            wasm_bindgen_futures::JsFuture::from(web_sys::window().unwrap().fetch_with_str(&url))
                .await
                .unwrap();
        let resp: web_sys::Response = resp.dyn_into().unwrap();

        let body = resp.body().unwrap();
        let reader = body.get_reader();

        let source = FetchByteSource { reader };
        let stream = ReadableStream::builder(source).spawn(spawn_local);

        let (_, reader) = stream.get_reader().unwrap();
        let mut received = 0;

        while let Ok(Some(chunk)) = reader.read().await {
            received += chunk.len();
            progress_el.set_inner_html(&format!("Progress: {} bytes", received));
        }

        progress_el.set_inner_html("Download complete!");
    });
}
