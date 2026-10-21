// Modules using the WebAssembly features the web build is compiled to need.
import { EXCEPTIONS_PROBE, SIMD_PROBE } from "./probes.js";

// The browser and system, by name, for the help page to pick its advice by.
function about() {
  const agent = navigator.userAgent;
  const browser = /Firefox\//.test(agent) ? "firefox"
    : /Chrome\/|Chromium\//.test(agent) ? "chromium"
    : /Safari\//.test(agent) ? "safari"
    : "other";
  const os = /Android/.test(agent) ? "android"
    : /iPhone|iPad/.test(agent) ? "ios"
    : /Mac OS X/.test(agent) ? "macos"
    : /Windows/.test(agent) ? "windows"
    : /Linux|X11/.test(agent) ? "linux"
    : "other";
  return { browser, os };
}

function runsWasm() {
  return typeof WebAssembly === "object"
    && WebAssembly.validate(SIMD_PROBE)
    && WebAssembly.validate(EXCEPTIONS_PROBE);
}

async function draws() {
  try {
    if (await navigator.gpu?.requestAdapter()) {
      return true;
    }
  } catch {
    // An adapter that cannot be asked for is none; WebGL2 may still draw.
  }
  const context = document.createElement("canvas").getContext("webgl2");
  context?.getExtension("WEBGL_lose_context")?.loseContext();
  return context !== null;
}

// Why bviz cannot run here, or null where it can.
async function missing() {
  if (!isSecureContext) {
    return "insecure";
  }
  if (!runsWasm()) {
    return "wasm";
  }
  if (!(await draws())) {
    return "graphics";
  }
  return null;
}

// Leaves for the help page on why bviz cannot run, with what the browser said
// of it where it said anything.
function help(reason, detail) {
  const given = new URLSearchParams({ reason, ...about() });
  if (detail) {
    given.set("detail", detail);
  }
  location.replace(`help.html#${given}`);
}

// Covers the page with what crashed bviz and a way to reload it: the panic or
// fault `error` that escaped it, or, where an abort left none, the panic it
// last recorded.
function crashed(error) {
  if (document.querySelector(".crashed")) {
    return;
  }
  const what = error ? error.message : globalThis.bvizLastPanic;
  const message = document.createElement("p");
  message.textContent = what ? `bviz crashed: ${what}` : "bviz crashed.";
  const reload = document.createElement("button");
  reload.textContent = "Reload";
  reload.addEventListener("click", () => location.reload());
  const overlay = document.createElement("div");
  overlay.className = "crashed";
  overlay.append(message, reload);
  document.body.append(overlay);
  reload.focus();
}

// Says that bviz cannot use more than one thread, the page not being served
// with the headers that let it share memory between threads.
function warnNotIsolated() {
  const message = document.createElement("span");
  message.textContent = "This page is not cross-origin isolated, so bviz cannot use more "
    + "than one thread.";
  const dismiss = document.createElement("button");
  dismiss.textContent = "Dismiss";
  const banner = document.createElement("div");
  banner.className = "banner";
  banner.setAttribute("role", "status");
  banner.append(message, dismiss);
  dismiss.addEventListener("click", () => banner.remove());
  document.body.append(banner);
}

// Whether `error` is a panic that escaped bviz, or the module failing.
function escaped(error) {
  return error?.name === "PanicError" || error instanceof WebAssembly.RuntimeError;
}

const reason = await missing();
if (reason) {
  help(reason);
} else {
  addEventListener("gpui:graphicsunavailable", (event) => help("graphics", event.message));
  addEventListener("bviz:failed", (event) => help("failed", event.message));
  addEventListener("bviz:crashed", () => crashed());
  addEventListener("error", (event) => escaped(event.error) && crashed(event.error));
  addEventListener("unhandledrejection", (event) => escaped(event.reason) && crashed(event.reason));

  const status = document.createElement("p");
  status.className = "status";
  status.textContent = "Loading bviz…";
  document.body.append(status);
  addEventListener("bviz:ready", () => status.remove(), { once: true });
  if (!crossOriginIsolated) {
    warnNotIsolated();
  }

  try {
    const { default: init } = await import("./bviz.js");
    await init();
  } catch (error) {
    if (escaped(error)) {
      crashed(error);
    } else {
      help("load", String(error));
    }
  }
}
