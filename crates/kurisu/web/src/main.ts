import "./app.css";
import { mount } from "svelte";
import App from "./App.svelte";

mount(App, { target: document.getElementById("app")! });

// PWA: cache the app shell (needs a secure context: localhost, 127.0.0.1 or https).
// `updateViaCache: "none"` skips the HTTP cache for sw.js, so a deploy's new
// worker (new cache name) is picked up on the next visit, not on some later day.
if ("serviceWorker" in navigator && window.isSecureContext) {
  navigator.serviceWorker.register("./sw.js", { updateViaCache: "none" }).catch(() => {});
}
