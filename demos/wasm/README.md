# whatwg_streams — wasm demos

Browser demos of the WHATWG Streams API, compiled from Rust to WebAssembly with
[`wasm-bindgen`](https://rustwasm.github.io/wasm-bindgen/) and driving the
[`whatwg_streams`](../../crates/whatwg_streams) crate. The set mirrors the WHATWG spec demos and
MDN's [`dom-examples/streams`](https://github.com/mdn/dom-examples/tree/main/streams)
gallery — runnable live at
[mdn.github.io/dom-examples/streams](https://mdn.github.io/dom-examples/streams/) —
reimplemented in Rust.

This gallery is its own Cargo workspace, separate from the root, because its demos build
`whatwg_streams` with the `local` feature while the native demo uses `send`, and the two are
mutually exclusive. Each demo is an isolated crate under `<name>/` with its own `index.html`
harness; `index.html` here is a landing page linking to all of them.

All commands below run from this directory (`demos/wasm/`).

## Prerequisites

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-pack      # or: brew install wasm-pack
```

## Build

One demo:

```bash
cd <name>
wasm-pack build --target web     # outputs ./pkg/
```

All of them:

```bash
./build.sh
```

## Run

Serve this directory (a static server is required — ES module imports and service
workers do not work over `file://`):

```bash
python3 -m http.server 8080
# open http://localhost:8080/
```

Open the site at `http://localhost:8080/` (or `http://127.0.0.1:8080/`), **not** the
`http://[::]:8080/` address the server may print. Service Workers and other secure-context
features are only enabled on `localhost` / `127.0.0.1` / `[::1]`, not the `[::]` wildcard
address — the `service-worker-stream` demo will report "Service Workers not supported"
otherwise.

## Demos

| Demo | Shows |
|------|-------|
| `invert-canvas` | Invert a canvas's pixels through a `TransformStream` |
| `grayscale-png` | Decode a PNG in Rust, grayscale it through a `TransformStream` |
| `png-chunks` | Stream a PNG over fetch and parse its chunk structure through a `TransformStream` |
| `append-child` | A `WritableStream` sink that appends a `<div>` per chunk |
| `streaming-element` | The same sink, triggered against a target element |
| `streaming-element-backpressure` | The sink throttled, with a queue high-water mark of 1 |
| `streaming-fetch-progress` | Wrap a fetch body reader as a `ReadableSource` and count bytes |
| `service-worker-stream` | A service worker streams a response; a Rust sink writes it to the DOM |

## Notes

- The demos use the crate's `local` feature (`default-features = false, features = ["local"]`),
  which is `Rc`-based and drops the `Send`/`Sync` bounds — the right fit for the browser's
  single-threaded `spawn_local`.
