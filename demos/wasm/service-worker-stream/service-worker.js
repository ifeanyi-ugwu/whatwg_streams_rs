self.addEventListener("install", (event) => {
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  self.clients.claim();
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);

  if (url.pathname === "/streaming-element") {
    event.respondWith(
      new Response(
        new ReadableStream({
          start(controller) {
            const encoder = new TextEncoder();
            let count = 0;
            let intervalId;

            intervalId = setInterval(() => {
              count++;
              const chunk = encoder.encode(`Chunk ${count}\n`);
              controller.enqueue(chunk);

              if (count >= 10) {
                clearInterval(intervalId);
                controller.close();
              }
            }, 500); // Increased to 500ms so you can see individual chunks
          },

          cancel() {
            // This will be called if the fetch is aborted
            console.log("Stream cancelled");
          },
        }),
        {
          headers: {
            "Content-Type": "text/plain",
            "Cache-Control": "no-cache",
          },
        }
      )
    );
  }
});

/*self.addEventListener("install", (event) => {
  console.log("Service Worker installing...");
  self.skipWaiting(); // Activate immediately
});

self.addEventListener("activate", (event) => {
  console.log("Service Worker activating...");
  event.waitUntil(
    self.clients.claim().then(() => {
      console.log("Service Worker now controls all pages");
    })
  );
});

self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);

  if (url.pathname === "/streaming-element") {
    console.log("Service Worker intercepting streaming request");

    event.respondWith(
      new Response(
        new ReadableStream({
          start(controller) {
            const encoder = new TextEncoder();
            let count = 0;
            let intervalId;

            intervalId = setInterval(() => {
              count++;
              const chunk = encoder.encode(`Chunk ${count}\n`);
              controller.enqueue(chunk);

              if (count >= 10) {
                clearInterval(intervalId);
                controller.close();
              }
            }, 500);
          },

          cancel() {
            console.log("Stream cancelled");
          },
        }),
        {
          headers: {
            "Content-Type": "text/plain",
            "Cache-Control": "no-cache",
          },
        }
      )
    );
  }
});
*/
