#!/usr/bin/env bash
# Build every demo crate to its own pkg/ with wasm-pack.
set -euo pipefail
cd "$(dirname "$0")"
for demo in invert-canvas grayscale-png png-chunks append-child streaming-element \
            streaming-element-backpressure streaming-fetch-progress service-worker-stream; do
  echo "==> $demo"
  (cd "$demo" && wasm-pack build --target web)
done
echo "Done. Serve from here with: python3 -m http.server 8080"
