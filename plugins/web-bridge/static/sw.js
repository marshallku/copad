// nestty web-bridge service worker.
// Owns two things: (1) PWA install lifecycle, (2) push events.
// Deliberately no offline caching — the SPA is small + cache complexity
// outweighs the rare offline benefit. If we later want offline-attach
// we'll add a precache step here.

self.addEventListener("install", (e) => {
  // Skip waiting so a fresh sw takes over immediately on reload.
  self.skipWaiting();
});

self.addEventListener("activate", (e) => {
  e.waitUntil(self.clients.claim());
});

// Push handler. Payload shape (set by the Rust side):
//   { title, body, tag, kind, url }
// Falling back to opaque defaults so a malformed payload still surfaces
// SOMETHING — silent push events are the worst debug experience.
self.addEventListener("push", (e) => {
  let data = {};
  try { data = e.data ? e.data.json() : {}; } catch { data = { body: (e.data && e.data.text()) || "" }; }
  const title = data.title || "nestty";
  const body = data.body || "";
  const tag = data.tag || ("nestty-" + Date.now());
  const url = data.url || "/";
  const opts = {
    body,
    tag,
    icon: "/icon.svg",
    badge: "/icon.svg",
    // Replace prior notifications with the same tag (e.g. successive
    // "Claude is waiting" events). renotify forces a sound/vibration
    // even when replacing.
    renotify: true,
    data: { url, kind: data.kind || "" },
  };
  e.waitUntil(self.registration.showNotification(title, opts));
});

// On tap, focus the existing dashboard tab if any, else open one.
self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const url = (e.notification.data && e.notification.data.url) || "/";
  e.waitUntil((async () => {
    const all = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const client of all) {
      // Prefer an already-open dashboard tab.
      if (client.url.includes(self.registration.scope)) {
        await client.focus();
        if ("navigate" in client) {
          try { await client.navigate(url); } catch {}
        }
        return;
      }
    }
    await self.clients.openWindow(url);
  })());
});
