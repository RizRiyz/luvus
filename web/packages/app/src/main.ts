import "./styles.css";
import { WebApp } from "./app.js";

const root = document.querySelector<HTMLElement>("#app");
if (!root) throw new Error("Missing app root");

const app = new WebApp(root);
void app.start();

if ("serviceWorker" in navigator && import.meta.env.PROD) {
  void navigator.serviceWorker.register("/sw.js");
}
