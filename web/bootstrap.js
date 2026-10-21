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

const reason = await missing();
if (reason) {
  help(reason);
} else {
  const status = document.createElement("p");
  status.className = "status";
  status.textContent = "Loading bviz…";
  document.body.append(status);
  addEventListener("bviz:ready", () => status.remove(), { once: true });

  try {
    const { default: init } = await import("./bviz.js");
    await init();
  } catch (error) {
    help("load", String(error));
  }
}
