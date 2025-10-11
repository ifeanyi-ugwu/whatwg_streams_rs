/*use wasm_bindgen::prelude::*;
use whatwg_streams::local::ReadableStream;

// Entry point for WASM
#[wasm_bindgen]
pub async fn run_local_stream() {
    let stream = ReadableStream::from_vec(vec![1, 2, 3]).spawn(wasm_bindgen_futures::spawn_local); // single-threaded future executor

    let (_, reader) = stream.get_reader().unwrap();

    while let Some(item) = reader.read().await.unwrap() {
        web_sys::console::log_1(&format!("Got: {}", item).into());
    }
}*/

/*use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_test::*;
use whatwg_streams::local::ReadableStream;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn test_local_stream() {
    spawn_local(async {
        let stream = ReadableStream::from_vec(vec![1, 2, 3]).spawn(spawn_local);

        let (_, reader) = stream.get_reader().unwrap();
        let mut collected = Vec::new();

        while let Some(item) = reader.read().await.unwrap() {
            collected.push(item);
        }

        assert_eq!(collected, vec![1, 2, 3]);
    });
}
*/

/*use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlElement;
use whatwg_streams::local::{ReadableStream, TransformStream, Transformer};

// A simple transformer that uppercases strings
struct UppercaseTransformer;

impl Transformer<&str, String> for UppercaseTransformer {
    async fn transform(
        &mut self,
        chunk: &str,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<String>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        controller.enqueue(chunk.to_uppercase())
    }
}

#[wasm_bindgen]
pub async fn run_stream_demo(element_id: &str) {
    let output_el = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into::<HtmlElement>()
        .unwrap();

    // Source stream
    let source = ReadableStream::from_vec(vec!["hello", "world", "from", "wasm"])
        .spawn(wasm_bindgen_futures::spawn_local);

    // Transform stream
    let transform =
        TransformStream::builder(UppercaseTransformer).spawn(wasm_bindgen_futures::spawn_local);

    // Pipe source through transform
    let output = source
        .pipe_through(transform, None)
        .spawn(wasm_bindgen_futures::spawn_local);

    let (_, mut reader) = output.get_reader().unwrap();

    // Read all chunks and append to page
    while let Some(chunk) = reader.read().await.unwrap() {
        let current = output_el.inner_html();
        output_el.set_inner_html(&format!("{}{}\n", current, chunk));
    }
}
*/

/*use serde_wasm_bindgen::from_value;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::HtmlElement;
use whatwg_streams::local::{ReadableStream, TransformStream, Transformer};

// Transformer: uppercase but fails randomly
struct UnstableUppercase;

impl Transformer<String, String> for UnstableUppercase {
    async fn transform(
        &mut self,
        chunk: String,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<String>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        if fastrand::f64() < 0.2 {
            return Err("Random error".into());
        }
        controller.enqueue(chunk.to_uppercase())
    }
}

#[wasm_bindgen]
pub async fn run_stream_demo(element_id: &str, items: JsValue) {
    let output_el = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(element_id)
        .unwrap()
        .dyn_into::<HtmlElement>()
        .unwrap();

    // Convert JS array into Vec<String>
    let input: Vec<String> = from_value(items).unwrap_or_default();

    let source = ReadableStream::from_vec(input).spawn(wasm_bindgen_futures::spawn_local);

    let transform =
        TransformStream::builder(UnstableUppercase).spawn(wasm_bindgen_futures::spawn_local);

    let output = source
        .pipe_through(transform, None)
        .spawn(wasm_bindgen_futures::spawn_local);

    let (_, reader) = output.get_reader().unwrap();

    loop {
        match reader.read().await {
            Ok(Some(chunk)) => {
                let current = output_el.inner_html();
                output_el.set_inner_html(&format!("{}{}\n", current, chunk));
            }
            Ok(None) => {
                // End of stream
                break;
            }
            Err(err) => {
                web_sys::console::error_1(&format!("Stream error: {:?}", err).into());
                let current = output_el.inner_html();
                output_el.set_inner_html(&format!("{}[ERROR: {:?}]\n", current, err));
                break;
            }
        }
    }
}
*/

/*use js_sys::Uint8ClampedArray;
use wasm_bindgen::Clamped;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};
use whatwg_streams::local::{ReadableStream, TransformStream, Transformer};

/*
// Transformer: invert pixel colors
struct InvertColors;

impl Transformer<Vec<u8>, Vec<u8>> for InvertColors {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        // Each chunk is RGBA pixel data
        for px in chunk.chunks_mut(4) {
            px[0] = 255 - px[0]; // R
            px[1] = 255 - px[1]; // G
            px[2] = 255 - px[2]; // B
            // Alpha (px[3]) left as-is
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

#[wasm_bindgen]
pub async fn run_stream_demo(canvas_id: &str) {
    // Get canvas + 2d context
    let canvas: HtmlCanvasElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();

    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into::<CanvasRenderingContext2d>()
        .unwrap();

    // Grab pixel data from canvas
    let image_data = ctx
        .get_image_data(0.0, 0.0, canvas.width().into(), canvas.height().into())
        .unwrap();
    //let pixels: Vec<u8> = js_sys::Uint8ClampedArray::new(&image_data.data()).to_vec();
    let pixels: Vec<u8> = Uint8ClampedArray::new(&image_data.data().into()).to_vec();

    // Source stream: just one chunk of pixels
    let source = ReadableStream::from_vec(vec![pixels]).spawn(wasm_bindgen_futures::spawn_local);

    // Transform stream: invert colors
    let transform = TransformStream::builder(InvertColors).spawn(wasm_bindgen_futures::spawn_local);

    // Pipe source through transform
    let output = source
        .pipe_through(transform, None)
        .spawn(wasm_bindgen_futures::spawn_local);

    let (_, reader) = output.get_reader().unwrap();

    // Read transformed pixels and put them back into canvas
    /*while let Some(chunk) = reader.read().await.unwrap() {
        let clamped = js_sys::Uint8ClampedArray::from(&chunk[..]);
        let new_image_data =
            ImageData::new_with_u8_clamped_array_and_sh(clamped, canvas.width(), canvas.height())
                .unwrap();
        ctx.put_image_data(&new_image_data, 0.0, 0.0).unwrap();
    }*/
    while let Ok(Some(chunk)) = reader.read().await {
        // chunk: Vec<u8>
        // create Clamped<&[u8]> view for ImageData constructor
        let clamped = Clamped(chunk.as_slice());
        let new_image_data =
            ImageData::new_with_u8_clamped_array_and_sh(clamped, canvas.width(), canvas.height())
                .unwrap();
        ctx.put_image_data(&new_image_data, 0.0, 0.0).unwrap();
    }
}
*/
// Transformer: invert pixel colors
struct InvertColors;
impl Transformer<Vec<u8>, Vec<u8>> for InvertColors {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        for px in chunk.chunks_mut(4) {
            px[0] = 255 - px[0];
            px[1] = 255 - px[1];
            px[2] = 255 - px[2];
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

// Transformer: grayscale
struct Grayscale;
impl Transformer<Vec<u8>, Vec<u8>> for Grayscale {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        for px in chunk.chunks_mut(4) {
            let gray = ((px[0] as u16 + px[1] as u16 + px[2] as u16) / 3) as u8;
            px[0] = gray;
            px[1] = gray;
            px[2] = gray;
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

// Transformer: brighten (adds +40 to each channel, clamped at 255)
struct Brighten;
impl Transformer<Vec<u8>, Vec<u8>> for Brighten {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        for px in chunk.chunks_mut(4) {
            px[0] = px[0].saturating_add(40);
            px[1] = px[1].saturating_add(40);
            px[2] = px[2].saturating_add(40);
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

#[wasm_bindgen]
pub async fn run_stream_demo(canvas_id: &str, mode: &str) {
    let canvas: HtmlCanvasElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();

    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into::<CanvasRenderingContext2d>()
        .unwrap();

    // Grab pixel data
    let image_data = ctx
        .get_image_data(0.0, 0.0, canvas.width().into(), canvas.height().into())
        .unwrap();
    let pixels: Vec<u8> = Uint8ClampedArray::new(&image_data.data().into()).to_vec();

    // Source stream
    let source = ReadableStream::from_vec(vec![pixels]).spawn(wasm_bindgen_futures::spawn_local);

    // Pick transform based on mode
    let transform = match mode {
        "invert" => TransformStream::builder(InvertColors).spawn(wasm_bindgen_futures::spawn_local),
        "grayscale" => TransformStream::builder(Grayscale).spawn(wasm_bindgen_futures::spawn_local),
        "brighten" => TransformStream::builder(Brighten).spawn(wasm_bindgen_futures::spawn_local),
        _ => TransformStream::builder(InvertColors).spawn(wasm_bindgen_futures::spawn_local),
    };

    // Pipe + apply
    let output = source
        .pipe_through(transform, None)
        .spawn(wasm_bindgen_futures::spawn_local);
    let (_, reader) = output.get_reader().unwrap();

    while let Ok(Some(chunk)) = reader.read().await {
        let clamped = Clamped(chunk.as_slice());
        let new_image_data =
            ImageData::new_with_u8_clamped_array_and_sh(clamped, canvas.width(), canvas.height())
                .unwrap();
        ctx.put_image_data(&new_image_data, 0.0, 0.0).unwrap();
    }
}
*/
use js_sys::Uint8ClampedArray;
use wasm_bindgen::Clamped;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};
use whatwg_streams::local::{ReadableStream, TransformStream, Transformer};

// Transformer: invert pixel colors
struct InvertColors;

impl Transformer<Vec<u8>, Vec<u8>> for InvertColors {
    async fn transform(
        &mut self,
        mut chunk: Vec<u8>,
        controller: &mut whatwg_streams::local::TransformStreamDefaultController<Vec<u8>>,
    ) -> Result<(), whatwg_streams::local::error::StreamError> {
        for px in chunk.chunks_mut(4) {
            px[0] = 255 - px[0]; // R
            px[1] = 255 - px[1]; // G
            px[2] = 255 - px[2]; // B
        }
        controller.enqueue(chunk);
        Ok(())
    }
}

#[wasm_bindgen]
pub async fn invert_canvas(canvas_id: &str) {
    // Get canvas + 2d context
    let canvas: HtmlCanvasElement = web_sys::window()
        .unwrap()
        .document()
        .unwrap()
        .get_element_by_id(canvas_id)
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();

    let ctx: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .unwrap()
        .unwrap()
        .dyn_into::<CanvasRenderingContext2d>()
        .unwrap();

    // Grab pixel data
    let image_data = ctx
        .get_image_data(0.0, 0.0, canvas.width().into(), canvas.height().into())
        .unwrap();
    let pixels: Vec<u8> = Uint8ClampedArray::new(&image_data.data().into()).to_vec();

    // Stream through transformer
    let source = ReadableStream::from_vec(vec![pixels]).spawn(wasm_bindgen_futures::spawn_local);
    let transform = TransformStream::builder(InvertColors).spawn(wasm_bindgen_futures::spawn_local);
    let output = source
        .pipe_through(transform, None)
        .spawn(wasm_bindgen_futures::spawn_local);

    let (_, reader) = output.get_reader().unwrap();

    // Write back
    while let Ok(Some(chunk)) = reader.read().await {
        let clamped = Clamped(chunk.as_slice());
        let new_image_data =
            ImageData::new_with_u8_clamped_array_and_sh(clamped, canvas.width(), canvas.height())
                .unwrap();
        ctx.put_image_data(&new_image_data, 0.0, 0.0).unwrap();
    }
}
