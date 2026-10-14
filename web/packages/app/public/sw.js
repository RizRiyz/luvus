// Luvus Web no longer uses a service worker: the page is only useful while its
// bridge is running, and a cached copy could outlive the bridge that served it.
// Browsers that registered the earlier worker fetch this script when they check
// for updates; it deletes that worker's caches and unregisters itself.
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((key) => key.startsWith("luvus-web")).map((key) => caches.delete(key))))
      .then(() => self.registration.unregister()),
  );
});
