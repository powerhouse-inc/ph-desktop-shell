// The update notice (update.rs). The shell tells every page where an update
// stands; this draws it: a card that drops from the top when a newer version is
// found, folds into a pill after a few seconds, shows the download's progress,
// and says when the app is about to restart. It draws nothing in an app without
// a release feed, and nothing in an app that draws its own (takeOver).
//
// The same state is there for the page: window.phDesktop.update
//   .state()            the status now
//   .check()            ask the feed now
//   .install()          download, install and restart
//   .subscribe(fn)      called at every change; returns the way to stop
//   .takeOver()         the page draws the notice itself; this one goes away
(() => {
  if (window.top !== window) return; // the top page only, never a frame in it
  const invoke = (command, args) => window.__TAURI_INTERNALS__.invoke(command, args);

  const SHOW_MS = 4000;
  let view = { enabled: false, state: "idle" };
  let folded = false; // the card became the pill
  let shownFor = null; // the version the card dropped for
  let ownedByPage = false;
  let foldTimer;
  const listeners = new Set();

  const CSS = `
    :host { all: initial; position: fixed; top: 12px; left: 0; right: 0; z-index: 2147483000;
      display: flex; justify-content: center; pointer-events: none;
      font: 13px/1.45 system-ui, -apple-system, "Segoe UI", sans-serif;
      --bg: #ffffff; --fg: #1a1f1c; --muted: #5b655f; --line: #d9dfdb; --accent: #1f6f4a; --on-accent: #ffffff; --bad: #b3261e; }
    @media (prefers-color-scheme: dark) {
      :host { --bg: #1b211e; --fg: #eef2ef; --muted: #a7b2ab; --line: #333c37; --accent: #8fd3ad; --on-accent: #0d1411; --bad: #ffb4ab; }
    }
    .card, .pill { pointer-events: auto; background: var(--bg); color: var(--fg);
      border: 1px solid var(--line); box-shadow: 0 6px 24px rgb(0 0 0 / .16); }
    .card { width: min(380px, calc(100vw - 32px)); border-radius: 12px; padding: 14px 16px; animation: drop .28s ease-out; }
    .card[data-bad] { border-color: var(--bad); }
    .title { margin: 0 0 4px; font-weight: 600; font-size: 14px; }
    .text { margin: 0; color: var(--muted); overflow-wrap: anywhere; }
    .actions { display: flex; gap: 8px; margin-top: 12px; }
    button { font: inherit; border-radius: 8px; padding: 6px 12px; cursor: pointer;
      border: 1px solid var(--line); background: transparent; color: var(--fg); }
    button.primary { background: var(--accent); border-color: var(--accent); color: var(--on-accent); font-weight: 600; }
    button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
    .pill { position: relative; overflow: hidden; border-radius: 999px; padding: 5px 12px; animation: fold .2s ease-out; }
    .pill[disabled] { cursor: default; }
    .bar { position: absolute; left: 0; bottom: 0; height: 2px; width: 100%; background: var(--accent);
      transform-origin: left; transition: transform .2s linear; }
    @keyframes drop { from { transform: translateY(-24px); opacity: 0; } }
    @keyframes fold { from { transform: scale(.94); opacity: 0; } }
    @media (prefers-reduced-motion: reduce) { .card, .pill { animation: none; } .bar { transition: none; } }
  `;

  let host, root;
  function mount() {
    if (host || !document.documentElement) return;
    host = document.createElement("div");
    host.setAttribute("data-ph-update", "");
    root = host.attachShadow({ mode: "closed" });
    try {
      // A constructed sheet is not subject to the page's style-src policy.
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(CSS);
      root.adoptedStyleSheets = [sheet];
    } catch {
      const style = document.createElement("style");
      style.textContent = CSS;
      root.append(style);
    }
    document.documentElement.append(host);
  }

  const el = (tag, className, text) => {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const button = (label, onClick, primary) => {
    const node = el("button", primary ? "primary" : "", label);
    node.type = "button";
    node.addEventListener("click", onClick);
    return node;
  };
  const megabytes = (bytes) => `${Math.round(bytes / 1048576)} MB`;

  function card({ title, text, actions = [], bad = false, live = "polite" }) {
    const node = el("div", "card");
    node.setAttribute("role", bad ? "alert" : "status");
    node.setAttribute("aria-live", live);
    if (bad) node.setAttribute("data-bad", "");
    node.append(el("p", "title", title), el("p", "text", text));
    if (actions.length) {
      const row = el("div", "actions");
      row.append(...actions);
      node.append(row);
    }
    // The card stays while the pointer or the focus is on it.
    const hold = () => clearTimeout(foldTimer);
    node.addEventListener("pointerenter", hold);
    node.addEventListener("focusin", hold);
    node.addEventListener("pointerleave", scheduleFold);
    node.addEventListener("focusout", scheduleFold);
    return node;
  }

  function scheduleFold() {
    clearTimeout(foldTimer);
    if (view.state !== "available" || folded) return;
    foldTimer = setTimeout(() => {
      folded = true;
      draw();
    }, SHOW_MS);
  }

  const install = () => invoke("update_install").catch(() => undefined);
  const later = () => {
    folded = true;
    draw();
  };

  function draw() {
    if (!host && (ownedByPage || !view.enabled || view.state === "idle")) return;
    mount();
    if (!root) return;
    for (const node of [...root.children]) if (node.tagName !== "STYLE") node.remove();
    if (ownedByPage || !view.enabled || view.state === "idle") return;

    const { product = "The app", version, state } = view;
    if (state === "available" && !folded) {
      root.append(
        card({
          title: `${product} ${version} is available`,
          text: "Update downloads it, installs it and restarts the app. Your data stays where it is.",
          actions: [button("Update now", install, true), button("Later", later)],
        }),
      );
      scheduleFold();
      return;
    }
    if (state === "installing") {
      root.append(
        card({
          title: `Installing ${product} ${version}`,
          text: "The app restarts by itself in a moment. Your computer may ask for your password.",
          live: "assertive",
        }),
      );
      return;
    }
    if (state === "failed" && !folded) {
      root.append(
        card({
          title: "The update did not go in",
          text: `${view.error || "Something went wrong."} ${product} keeps running as it was.`,
          actions: [button("Try again", install, true), button("Later", later)],
          bad: true,
        }),
      );
      return;
    }

    // The pill: an update waiting, or its download under way.
    const downloading = state === "downloading";
    const share = downloading && view.total ? Math.min(1, view.received / view.total) : null;
    const label = downloading
      ? share === null
        ? "Downloading the update…"
        : `Downloading ${Math.round(share * 100)}% · ${megabytes(view.received)}`
      : `Update to ${version}`;
    const pill = el("button", "pill", label);
    pill.type = "button";
    pill.setAttribute("aria-live", "polite");
    if (downloading) pill.disabled = true;
    else {
      pill.title = `Download ${product} ${version}, install it and restart`;
      pill.addEventListener("click", install);
    }
    if (share !== null) {
      const bar = el("span", "bar");
      bar.style.transform = `scaleX(${share})`;
      pill.append(bar);
    }
    root.append(pill);
  }

  // Called by the shell at every change, and with the answer to the first ask.
  window.__phUpdate = (next) => {
    if (!next || typeof next !== "object") return;
    view = next;
    // A newer version than the card dropped for drops it again; a failure too.
    if (view.state === "available" && view.version !== shownFor) {
      shownFor = view.version;
      folded = false;
    }
    if (view.state === "failed") folded = false;
    draw();
    for (const listener of listeners) {
      try {
        listener(view);
      } catch {
        // a page's listener is the page's concern
      }
    }
  };

  const bridge = (window.phDesktop ??= {});
  bridge.update = {
    state: () => invoke("update_state"),
    check: () => invoke("update_check"),
    install: () => invoke("update_install"),
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    takeOver() {
      ownedByPage = true;
      draw();
    },
  };

  const start = () => invoke("update_state").then(window.__phUpdate).catch(() => undefined);
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
})();
