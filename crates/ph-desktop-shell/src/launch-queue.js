// Polyfill of the PWA File Handling API for WebView2, which does not implement
// it. Connect registers a `window.launchQueue` consumer on mount
// (apps/connect/src/utils/launch-queue.ts) and imports whatever it is handed;
// this supplies the launches Chromium would deliver to an installed PWA, so
// nothing in Connect has to know it is running inside Tauri.
//
// Injected as a Tauri initialization script: it runs before any page script, on
// every document, including after the shell navigates from the loading page to
// Connect.
(() => {
  // WebView2 *does* expose window.launchQueue, but it is inert here: the File
  // Handling API only ever delivers to an installed PWA, and an embedded webview
  // is not one. So this replaces it instead of deferring to it. An earlier
  // version bailed out when it found one, which silently disabled the shim.
  const native = window.launchQueue;

  let consumer = null;

  /**
   * Mirrors into the shell's log file. A release build has no console and the
   * page may fail before anyone can open devtools, so the interesting steps have
   * to leave a trace somewhere durable.
   */
  // Also kept in-page, because the IPC this would otherwise rely on is one of
  // the things that can be broken. The shell reads this back with
  // `eval_with_callback`, which does not go through the IPC ACL at all.
  window.__phLog = window.__phLog || [];

  function report(level, message) {
    window.__phLog.push(`${level}: ${message}`);
    console[level === "error" ? "error" : "log"](`[ph-win] ${message}`);
    try {
      // A rejected invoke is a promise rejection, not a throw, so it needs the
      // catch too — an ACL denial must not surface as an unhandled rejection.
      window.__TAURI_INTERNALS__?.invoke("log_from_webview", { level, message })
        ?.catch((error) => window.__phLog.push(`ipc-log-failed: ${error}`));
    } catch (error) {
      window.__phLog.push(`ipc-log-threw: ${error}`);
    }
  }

  const shim = {
    setConsumer(next) {
      consumer = next;
      report("info", "launchQueue consumer registered");
      // Connect wires the consumer in a mount effect, so this is the point at
      // which a cold-start launch can finally be delivered.
      void take();
    },
  };

  /**
   * Drains the shell-side queue and hands the files to the consumer.
   *
   * No-op until a consumer exists. That is what keeps the loading page from
   * eating a launch: the shell's queue stays put until the document that can
   * actually import it — Connect — is the one asking.
   */
  async function take() {
    if (!consumer) {
      report("info", "take() with no consumer yet; leaving the queue alone");
      return;
    }

    const internals = window.__TAURI_INTERNALS__;
    if (!internals) {
      report("error", "__TAURI_INTERNALS__ missing; cannot reach the shell");
      return;
    }

    let metas;
    try {
      metas = await internals.invoke("take_launch_files");
    } catch (error) {
      report("error", `take_launch_files rejected: ${error}`);
      return;
    }
    report("info", `take_launch_files returned ${metas?.length ?? 0} file(s)`);
    if (!metas?.length) return;

    const files = [];
    for (const meta of metas) {
      try {
        const bytes = await internals.invoke("read_launch_file", { id: meta.id });
        const size = bytes?.byteLength ?? bytes?.length ?? 0;
        const file = new File([bytes], meta.name, { lastModified: meta.lastModified });
        // Connect only reads `kind` and `getFile()` off the handle, so a
        // duck-typed one is enough; a real FileSystemFileHandle cannot be
        // constructed from script anyway.
        files.push({ kind: "file", name: meta.name, getFile: () => Promise.resolve(file) });
        report("info", `read ${meta.name}: ${size} bytes`);
      } catch (error) {
        report("error", `read_launch_file(${meta.id}) failed: ${error}`);
      }
    }

    // One failed file must not drop the rest of the batch.
    if (!files.length) return;
    report("info", `delivering ${files.length} file(s) to Connect`);
    try {
      consumer({ files });
      report("info", "consumer returned");
    } catch (error) {
      report("error", `Connect's launch consumer threw: ${error}`);
    }
  }

  // Warm launches: the shell calls this directly after queueing, rather than
  // going through the event plugin, which a raw initialization script cannot
  // reach without depending on Tauri's internal IPC shape.
  window.__phTakeLaunchFiles = () => {
    void take();
  };

  // `launchQueue` is a readonly attribute on Window, so `window.launchQueue = …`
  // is silently dropped in sloppy mode. defineProperty is what actually replaces it.
  try {
    Object.defineProperty(window, "launchQueue", {
      value: shim,
      configurable: true,
      writable: true,
    });
  } catch (error) {
    window.launchQueue = shim;
  }

  report(
    "info",
    `shim installed on ${location.origin} (native launchQueue was ${typeof native}; ` +
      `replaced: ${window.launchQueue === shim})`,
  );
})();
