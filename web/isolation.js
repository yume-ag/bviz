// The service worker that makes the site cross-origin isolated on a host that
// cannot send the headers that do so, by adding them to each of its responses.

// Embedding requires every cross-origin resource to opt in; the site loads
// only its own, which need not.
const ISOLATION = [
  ["Cross-Origin-Opener-Policy", "same-origin"],
  ["Cross-Origin-Embedder-Policy", "require-corp"],
];

self.addEventListener("fetch", (event) => {
  if (new URL(event.request.url).origin === location.origin) {
    event.respondWith(isolated(event.request));
  }
});

async function isolated(request) {
  const response = await fetch(request);
  // A navigation follows its redirect itself, through this worker again.
  if (response.type === "opaqueredirect") {
    return response;
  }
  const headers = new Headers(response.headers);
  for (const [name, value] of ISOLATION) {
    headers.set(name, value);
  }
  const { status, statusText } = response;
  return new Response(response.body, { status, statusText, headers });
}
