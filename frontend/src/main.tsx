import React from "react";
import ReactDOM from "react-dom/client";import "katex/dist/katex.min.css";
import App from "./App";

// Register PWA service worker for offline caching + installability.
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register("/sw.js");
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
);
