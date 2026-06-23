use js_sys::{Function, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlElement;
use whatwg_streams::{
    ReadableSource, ReadableStream, ReadableStreamDefaultController, StreamResult,
};

/// Wraps a browser `ReadableStreamDefaultReader` (from a fetch response body) as a
/// `ReadableSource`, pulling one `{ done, value }` result per `pull`.
struct FetchByteSource {
    reader: Object,
}

impl ReadableSource<Vec<u8>> for FetchByteSource {
    async fn pull(
        &mut self,
        controller: &mut ReadableStreamDefaultController<Vec<u8>>,
    ) -> StreamResult<()> {
        let read_fn: Function = Reflect::get(&self.reader, &JsValue::from_str("read"))
            .unwrap()
            .dyn_into()
            .unwrap();
        let promise: js_sys::Promise = read_fn.call0(&self.reader).unwrap().dyn_into().unwrap();
        let result = wasm_bindgen_futures::JsFuture::from(promise).await.unwrap();

        // A finished reader yields `{ done: true, value: undefined }` — `value` is
        // undefined, not null, so the stream must be closed on `done`, not on a null value.
        let done = Reflect::get(&result, &JsValue::from_str("done"))
            .unwrap()
            .as_bool()
            .unwrap_or(false);
        if done {
            controller.close()?;
        } else {
            let value = Reflect::get(&result, &JsValue::from_str("value")).unwrap();
            controller.enqueue(Uint8Array::new(&value).to_vec())?;
        }
        Ok(())
    }
}

#[wasm_bindgen]
pub fn run_fetch_progress_demo(url: String, total_bytes: f64, progress_id: String) {
    let document = web_sys::window().unwrap().document().unwrap();
    let progress_el: HtmlElement = document
        .get_element_by_id(&progress_id)
        .unwrap()
        .dyn_into()
        .unwrap();

    spawn_local(async move {
        let response = match wasm_bindgen_futures::JsFuture::from(
            web_sys::window().unwrap().fetch_with_str(&url),
        )
        .await
        {
            Ok(r) => r.dyn_into::<web_sys::Response>().unwrap(),
            Err(_) => {
                progress_el.set_text_content(Some("Fetch failed (network or CORS)."));
                return;
            }
        };

        // Prefer the caller-supplied total. A cross-origin response hides Content-Length
        // from JS unless the server allows it via Access-Control-Expose-Headers, so
        // reading it only works for same-origin or explicitly-exposing servers.
        let total: Option<f64> = if total_bytes > 0.0 {
            Some(total_bytes)
        } else {
            response
                .headers()
                .get("content-length")
                .ok()
                .flatten()
                .and_then(|s| s.parse().ok())
        };

        let reader = response.body().unwrap().get_reader();
        let stream = ReadableStream::builder(FetchByteSource { reader }).spawn(spawn_local);
        let (_lock, reader) = stream.get_reader().unwrap();

        let mut received = 0usize;
        while let Ok(Some(chunk)) = reader.read().await {
            received += chunk.len();
            let msg = match total {
                Some(t) if t > 0.0 => format!("Progress: {:.0}%", received as f64 / t * 100.0),
                _ => format!("Progress: {received} bytes"),
            };
            progress_el.set_text_content(Some(&msg));
        }
        progress_el.set_text_content(Some("Download complete!"));
    });
}
