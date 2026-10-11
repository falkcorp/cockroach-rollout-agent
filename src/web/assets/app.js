// file: src/web/assets/app.js
// version: 1.0.0
// guid: b998b76a-5bbe-4ad9-8d5e-49d09827b759
// last-edited: 2026-10-11

// Refreshes <main id="main"> every 10 seconds. Skips while the operator is
// typing so a refresh never wipes a half-filled form. The page works fully
// without this file.
(function () {
  "use strict";

  function editing() {
    var el = document.activeElement;
    return !!el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT");
  }

  function refresh() {
    if (editing()) return;
    fetch("/", { credentials: "same-origin", cache: "no-store" })
      .then(function (response) {
        if (response.redirected && new URL(response.url).pathname === "/login") {
          window.location.assign("/login");
          return null;
        }
        return response.ok ? response.text() : null;
      })
      .then(function (html) {
        if (!html || editing()) return;
        var next = new DOMParser().parseFromString(html, "text/html").getElementById("main");
        var current = document.getElementById("main");
        if (next && current) current.replaceWith(next);
      })
      .catch(function () {});
  }

  if (document.getElementById("main")) setInterval(refresh, 10000);
})();
