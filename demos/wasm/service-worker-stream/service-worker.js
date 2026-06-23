self.addEventListener("install", () => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

// Respond to /streaming-element with a stream that emits one chunk every 500ms.
self.addEventListener("fetch", (event) => {
  // Scope-relative match so it works whether the demo is served from the site root
  // or from demos/service-worker-stream/.
  const url = new URL(event.request.url);
  if (!url.pathname.endsWith("/streaming-element")) return;

  event.respondWith(
    new Response(
      new ReadableStream({
        start(controller) {
          const encoder = new TextEncoder();
          let count = 0;
          const id = setInterval(() => {
            count++;
            controller.enqueue(encoder.encode(`Chunk ${count}\n`));
            if (count >= 10) {
              clearInterval(id);
              controller.close();
            }
          }, 500);
        },
      }),
      { headers: { "Content-Type": "text/plain", "Cache-Control": "no-cache" } }
    )
  );
});
