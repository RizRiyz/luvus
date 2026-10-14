import "./styles.css";
import { WebApp } from "./app.js";

const root = document.querySelector<HTMLElement>("#app");
if (!root) throw new Error("Missing app root");

const app = new WebApp(root);
void app.start();

void removeLegacyServiceWorker().catch(() => {});

/**
 * Earlier versions cached the page in a service worker, which could serve a
 * stale copy. The old worker keeps controlling this page load and caching
 * what it fetches, so it is replaced rather than only unregistered: the
 * current `sw.js` has no fetch handler, and on activation it deletes those
 * caches and unregisters itself.
 */
async function removeLegacyServiceWorker(): Promise<void> {
  if (!("serviceWorker" in navigator)) return;
  const registrations = await navigator.serviceWorker.getRegistrations();
  await Promise.all(registrations.map(async (registration) => {
    try {
      await registration.update();
    } catch {
      await registration.unregister();
    }
  }));
}
