// copad web-bridge service worker: PWA install lifecycle and push events. No offline caching.

self.addEventListener("install", (e) => {
  self.skipWaiting();
});

self.addEventListener("activate", (e) => {
  e.waitUntil(self.clients.claim());
});

self.addEventListener("push", (e) => {
  let data = {};
  try { data = e.data ? e.data.json() : {}; } catch { data = { body: (e.data && e.data.text()) || "" }; }
  const title = data.title || "copad";
  const body = data.body || "";
  const tag = data.tag || ("copad-" + Date.now());
  const url = data.url || "/";
  const opts = {
    body,
    tag,
    icon: "/icon.svg",
    badge: "/icon.svg",
    // Same tag replaces the prior notification; renotify still sounds/vibrates.
    renotify: true,
    data: { url, kind: data.kind || "" },
  };
  e.waitUntil(self.registration.showNotification(title, opts));
});

self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const url = (e.notification.data && e.notification.data.url) || "/";
  e.waitUntil((async () => {
    const all = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
    for (const client of all) {
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
