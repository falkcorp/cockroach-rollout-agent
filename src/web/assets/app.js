// file: src/web/assets/app.js
// version: 1.1.0
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

  // When the dashboard cannot be refreshed, never leave stale data looking
  // live: flag it and disable every form until a fresh page arrives.
  function markStale(reason) {
    var main = document.getElementById("main");
    if (!main) return;
    var banner = document.getElementById("stale-banner");
    if (!banner) {
      banner = document.createElement("p");
      banner.id = "stale-banner";
      banner.className = "banner";
      main.insertBefore(banner, main.firstChild);
    }
    banner.textContent = "Connection lost (" + reason + "); data as of " +
      new Date().toLocaleTimeString() + " may be out of date.";
    main.querySelectorAll("button, input").forEach(function (el) { el.disabled = true; });
  }

  function refresh() {
    if (editing()) return;
    fetch("/", { credentials: "same-origin", cache: "no-store" })
      .then(function (response) {
        if (response.redirected && new URL(response.url).pathname === "/login") {
          window.location.assign("/login");
          return null;
        }
        // Error pages such as the 503 "database unreachable" banner also
        // carry #main, so swap them in like any other page.
        return response.text().then(function (html) {
          return { ok: response.ok, status: response.status, html: html };
        });
      })
      .then(function (page) {
        if (!page || editing()) return;
        var next = new DOMParser().parseFromString(page.html, "text/html").getElementById("main");
        var current = document.getElementById("main");
        if (next && current) {
          current.replaceWith(next);
        } else if (!page.ok) {
          markStale("HTTP " + page.status);
        }
      })
      .catch(function () { markStale("agent unreachable"); });
  }

  if (document.getElementById("main")) setInterval(refresh, 10000);
})();
