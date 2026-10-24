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

// The help page's address for why bviz cannot run, with what the browser said
// of it where it said anything.
function helpAddress(reason, detail) {
  const given = new URLSearchParams({ reason, ...about() });
  if (detail) {
    given.set("detail", detail);
  }
  return `help.html#${given}`;
}

function help(reason, detail) {
  location.replace(helpAddress(reason, detail));
}

// Covers the page with `message`, text and elements, and a way to reload
// bviz, once bviz has ended for good.
function ended(...message) {
  if (document.querySelector(".ended")) {
    return;
  }
  const text = document.createElement("p");
  text.append(...message);
  const reload = document.createElement("button");
  reload.textContent = "Reload";
  reload.addEventListener("click", () => location.reload());
  const overlay = document.createElement("div");
  overlay.className = "ended";
  overlay.append(text, reload);
  document.body.append(overlay);
  reload.focus();
}

// The share of its size by which bviz's memory must still be able to grow for
// a crash not to count as running out of memory: an allocation that fails
// typically doubles a buffer that holds much of the memory.
const HEADROOM = 1 / 2;

// The module's memory, from when `init` resolves, after bviz's `main` has run.
let memory;

// The panic bviz last recorded, kept until the task that recorded it ends, so a
// crash in that task is the panic's, and one in a later task is not a panic
// bviz caught before it.
let lastPanic;
Object.defineProperty(globalThis, "bvizLastPanic", {
  get: () => lastPanic,
  set(message) {
    lastPanic = message;
    setTimeout(() => {
      lastPanic = undefined;
    });
  },
});

// Whether bviz's memory cannot grow by HEADROOM of its size. Asking it to grow
// meets whichever limit binds it, its declared maximum or the browser's own,
// and costs nothing once bviz has crashed.
function exhausted() {
  const pages = memory.grow(0);
  try {
    memory.grow(Math.ceil(pages * HEADROOM));
    return false;
  } catch {
    return true;
  }
}

// Tells what crashed bviz: the panic or fault `error` that escaped it, or,
// where an abort left none, the panic it last recorded. A fault that is no
// panic, at a memory that cannot grow, is running out of memory: a failed
// allocation aborts with a bare trap and runs no panic hook.
function crashed(error) {
  const panicked = error?.name === "PanicError" || lastPanic !== undefined;
  if (error instanceof WebAssembly.RuntimeError && !panicked && memory && exhausted()) {
    const desktop = document.createElement("a");
    desktop.href = helpAddress("memory");
    desktop.textContent = "bviz on the desktop";
    ended("bviz ran out of the memory the browser gives it. For large builds, run ", desktop, ".");
    return;
  }
  const what = lastPanic ?? error?.message;
  ended(what ? `bviz crashed: ${what}` : "bviz crashed.");
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
  addEventListener("bviz:closed", () => ended("bviz closed."));
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
    ({ memory } = await init());
  } catch (error) {
    if (escaped(error)) {
      crashed(error);
    } else {
      help("load", String(error));
    }
  }
}
