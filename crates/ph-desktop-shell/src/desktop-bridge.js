// What a page can ask of the shell. Connect's sign-ins look for
// `window.phDesktop` (packages/reactor-browser/src/renown/session.ts) and keep
// their redirect without it, so this runs before any page script.
(() => {
  window.phDesktop = {
    openExternal: (url) =>
      window.__TAURI_INTERNALS__.invoke("open_external", { url }),
  };
})();
