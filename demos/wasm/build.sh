#!/usr/bin/env bash
# Build every demo crate to its own pkg/ with wasm-pack.
set -euo pipefail

# VS Code's Run button (and other non-login shells) don't source the shell
# profile, so cargo and Homebrew binaries land off PATH. Add the usual spots.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
command -v wasm-pack >/dev/null || {
  echo "wasm-pack not found. Install it: cargo install wasm-pack (or brew install wasm-pack)" >&2
  exit 1
}

cd "$(dirname "$0")"
for demo in invert-canvas grayscale-png png-chunks append-child streaming-element \
            streaming-element-backpressure streaming-fetch-progress service-worker-stream; do
  echo "==> $demo"
  (cd "$demo" && wasm-pack build --target web)
done
echo "Done. Serve from here with: python3 -m http.server 8080"
