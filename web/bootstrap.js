import init from "./bviz.js";

const status = document.createElement("p");
status.className = "status";
status.textContent = "Loading bviz…";
document.body.append(status);
addEventListener("bviz:ready", () => status.remove(), { once: true });

await init();
