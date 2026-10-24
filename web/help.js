// Shows only the advice that fits the reason, browser and system the page was
// sent here with; without them, it shows all of it.
const given = new URLSearchParams(location.hash.slice(1));
const reason = given.get("reason");
const browser = given.get("browser");
const os = given.get("os");
const detail = given.get("detail");

if (reason) {
  for (const section of document.querySelectorAll("[data-reason]")) {
    section.hidden = section.dataset.reason !== reason;
  }
}
if (browser) {
  for (const tip of document.querySelectorAll("[data-browser]")) {
    const fits = tip.dataset.browser === browser
      && (tip.dataset.os === undefined || tip.dataset.os === os);
    tip.hidden = !fits;
  }
}
if (detail) {
  const reported = document.querySelector("[data-detail]");
  reported.querySelector("code").textContent = detail;
  reported.hidden = false;
}
