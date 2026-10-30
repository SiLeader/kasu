// Progressive enhancement only: every action also works without JavaScript.
const $$ = (selector, root = document) => Array.from(root.querySelectorAll(selector));

// Line ranges: shift-click extends a #Lx selection to #Lx-Ly.
function highlightLines() {
  $$("tr.sel").forEach((row) => row.classList.remove("sel"));
  const match = /^#L(\d+)(?:-L(\d+))?$/.exec(location.hash);
  if (!match) return;
  const from = Number(match[1]);
  const to = Number(match[2] || match[1]);
  for (let n = Math.min(from, to); n <= Math.max(from, to); n++) {
    const row = document.getElementById("L" + n);
    if (row) row.classList.add("sel");
  }
}
let anchor = null;
document.addEventListener("click", (event) => {
  const link = event.target.closest("table.source td.ln a");
  if (!link) return;
  const line = Number(link.getAttribute("href").slice(2));
  if (event.shiftKey && anchor !== null) {
    event.preventDefault();
    const [a, b] = [Math.min(anchor, line), Math.max(anchor, line)];
    history.replaceState(null, "", `#L${a}-L${b}`);
    highlightLines();
  } else {
    anchor = line;
  }
});
window.addEventListener("hashchange", highlightLines);
highlightLines();

// Copy buttons.
document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy]");
  if (!button) return;
  try {
    await navigator.clipboard.writeText(button.dataset.copy);
    const label = button.textContent;
    button.textContent = "✓";
    setTimeout(() => (button.textContent = label), 1200);
  } catch (_) {}
});

// Relative times.
const relative = new Intl.RelativeTimeFormat(document.documentElement.lang || "en", { numeric: "auto" });
$$("time[data-relative]").forEach((element) => {
  const seconds = (Date.parse(element.getAttribute("datetime")) - Date.now()) / 1000;
  const units = [["year", 31536000], ["month", 2592000], ["day", 86400], ["hour", 3600], ["minute", 60]];
  for (const [unit, size] of units) {
    if (Math.abs(seconds) >= size) {
      element.textContent = relative.format(Math.round(seconds / size), unit);
      return;
    }
  }
  element.textContent = relative.format(0, "minute");
});

// Double submission: disable submit buttons once a form is sent.
document.addEventListener("submit", (event) => {
  const form = event.target;
  if (form.dataset.sent) {
    event.preventDefault();
    return;
  }
  form.dataset.sent = "1";
  setTimeout(() => $$("button[type=submit]", form).forEach((b) => (b.disabled = true)), 0);
});

// Inline comment forms in diffs.
document.addEventListener("click", (event) => {
  const toggle = event.target.closest("[data-toggle]");
  if (!toggle) return;
  event.preventDefault();
  const target = document.getElementById(toggle.dataset.toggle);
  if (target) {
    target.classList.toggle("hide");
    const field = target.querySelector("textarea");
    if (field) field.focus();
  }
});
$$("[data-collapsed]").forEach((element) => element.classList.add("hide"));

// Deferred loading of collapsed diffs.
document.addEventListener("toggle", async (event) => {
  const details = event.target;
  if (!(details instanceof HTMLDetailsElement) || !details.open || !details.dataset.load) return;
  const url = details.dataset.load;
  delete details.dataset.load;
  try {
    const response = await fetch(url, { credentials: "same-origin" });
    if (!response.ok) return;
    const html = await response.text();
    const parsed = new DOMParser().parseFromString(html, "text/html");
    const body = parsed.querySelector("[data-fragment]");
    if (body) details.querySelector("div").replaceChildren(...body.childNodes);
  } catch (_) {}
}, true);

// "y" jumps to the permalink of the current view.
document.addEventListener("keydown", (event) => {
  if (event.target.closest("input, textarea, select") || event.ctrlKey || event.metaKey || event.altKey) return;
  if (event.key === "y") {
    const permalink = document.querySelector("a[data-permalink]");
    if (permalink) location.href = permalink.getAttribute("href") + location.hash;
  }
});

// Each directory entry's latest change, loaded after the page.
const entries = document.querySelector("table[data-latest]");
if (entries) {
  fetch(entries.dataset.latest, { credentials: "same-origin" })
    .then((response) => (response.ok ? response.text() : null))
    .then((html) => {
      if (!html) return;
      const loaded = new DOMParser().parseFromString(html, "text/html").getElementById("entries");
      if (loaded) entries.replaceWith(loaded);
    })
    .catch(() => {});
}
