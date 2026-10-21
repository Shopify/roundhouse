// Smoke check for the Fractal pass explorer: drive /fractal/ in a real
// browser engine (Playwright/chromium) and assert the edit -> passes ->
// render loop over the compiler wasm's `fractal_stages` export.
//
// Asserts:
//   1. boots, compiles app/models/article.rb, and the last stage is
//      inline-enum-predicates with its rewrites, on the lines they replace;
//   2. the app's `draft?` in app/models/public/ is declined, with the reason;
//   3. editing Gemfile.lock to Active Record 7.0.8 re-runs the passes and
//      the pass declines instead of rewriting;
//   4. no console or page errors.
//
// Serve the PARENT (wasm/) as the web root, since the page imports ../lib/:
//   python3 -m http.server 8099    # run from wasm/
//   node verify-fractal.mjs        # (run from wasm/fractal/)

import { createRequire } from "node:module";
const require = createRequire(new URL("../../tests/browser_smoke/", import.meta.url).pathname);
const { chromium } = require("playwright");

const PAGE_URL = "http://localhost:8099/fractal/index.html";

const browser = await chromium.launch();
const page = await browser.newPage();
const logs = [];
page.on("console", (m) => logs.push(`[${m.type()}] ${m.text()}`));
page.on("pageerror", (e) => logs.push(`[pageerror] ${e.message}`));
page.on("crash", () => logs.push("[crash] the page's renderer process died (OOM?)"));

let failed = false;
const fail = (msg) => { console.error(`FAIL: ${msg}`); failed = true; };
const bail = (label) => (err) => {
  console.error(`\n=== ${label} — last 30 page log lines ===`);
  logs.slice(-30).forEach((l) => console.error(l));
  console.error(`\n=== ${label} ===`);
  console.error(err);
  process.exit(1);
};
process.on("uncaughtException", bail("uncaught exception"));
process.on("unhandledRejection", bail("unhandled rejection"));

const state = () => page.evaluate(() => ({
  status: document.getElementById("status").textContent,
  chips: [...document.querySelectorAll(".chip .name")].map((e) => e.textContent),
  added: [...document.querySelectorAll("#code .line.add .txt")].map((e) => e.textContent),
  removed: [...document.querySelectorAll("#code .line.del .txt")].map((e) => e.textContent),
  notes: document.getElementById("notes").innerText,
  file: document.getElementById("file").value,
}));

await page.goto(PAGE_URL, { waitUntil: "load" });
await page.waitForSelector("#status.ok", { timeout: 60000 });

console.log("=== boot ===");
let s = await state();
console.log(s.status, "|", s.chips.join(" → "), "|", s.file);
if (s.file !== "app/models/article.rb") fail(`expected article.rb to be compiled, got ${s.file}`);
if (s.chips[0] !== "source" || s.chips.at(-1) !== "inline-enum-predicates") fail(`unexpected stages: ${s.chips.join(", ")}`);
const rewrites = Number((s.status.match(/^(\d+) rewrite/) || [])[1]);
if (rewrites !== 4) fail(`expected 4 rewrites and no rewrite of the nested app predicate, got "${s.status}"`);
if (!s.added.some((t) => t.includes("(status_for_database == 1) && !(status_for_database == 2)")))
  fail(`listed? not rewritten: ${JSON.stringify(s.added)}`);
if (!s.added.some((t) => t.includes('(visibility_for_database == "members")')))
  fail(`visibility_members? not rewritten: ${JSON.stringify(s.added)}`);
if (s.added.length !== s.removed.length) fail(`a rewrite moved a line: ${s.added.length} added vs ${s.removed.length} removed`);
if (!/defines `?draft\?`? itself/.test(s.notes)) fail(`draft? not declined: ${s.notes}`);

console.log("=== edit Gemfile.lock to activerecord 7.0.8 ===");
await page.click("#srcTabs button[title='Gemfile.lock']");
await page.evaluate(() => {
  const e = document.getElementById("editor");
  e.value = e.value.replaceAll("8.1.1", "7.0.8");
  e.dispatchEvent(new Event("input"));
});
await page.waitForFunction(() => /^0 rewrite/.test(document.getElementById("status").textContent), { timeout: 30000 });
s = await state();
console.log(s.status);
if (!/7\.0\.8 is older than 7\.1/.test(s.notes)) fail(`no version decline: ${s.notes}`);

const realErrors = logs.filter((l) => /pageerror|\[error\]|\[crash\]/.test(l));
if (realErrors.length) {
  console.log("\n=== console errors ===");
  realErrors.forEach((l) => console.log(l));
  fail(`${realErrors.length} console/page error(s)`);
}

await browser.close();

if (failed) process.exit(1);
console.log("\nOK: Fractal pass explorer — stages, rewrites, declines and re-run on edit verified in a real browser tab.");
