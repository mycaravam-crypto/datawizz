const fs = require("node:fs");
const vm = require("node:vm");
const assert = require("node:assert/strict");

const html = fs.readFileSync("web/index.html", "utf8");
const scripts = [...html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/gi)];
assert.equal(scripts.length, 1, "Expected one embedded application script");
new vm.Script(scripts[0][1], { filename: "web/index.html" });

const ids = ["sql", "tabs", "objects", "filter", "columns", "run", "csv",
  "output", "result-filter", "prev", "next", "status", "theme", "connection-chooser"];
for (const id of ids) {
  assert.match(html, new RegExp('id="' + id + '"'), "Missing UI control: " + id);
}
assert.match(html, /\/api\/connections/, "Connection chooser must load server-provided profiles");
assert.match(html, /id="row-actions"[^>]*>[\\s\\S]*id="browse"[\\s\\S]*id="insert"/, "Browse and Insert must be in the table toolbar");
assert.doesNotMatch(html.match(/<div class="sidebar-bottom">[\\s\\S]*?<\\/div>/)?.[0] || "", /id="browse"|id="insert"/, "Sidebar must not contain table actions");
assert.match(html, /aria-label=/, "UI should have accessible labels");
assert.match(html, /:focus-visible/, "Missing keyboard focus styles");
assert.match(html, /textContent/, "Dynamic SQL/database strings must render as text");
const csvFn = scripts[0][1].match(/function csvValue\(v\)\s*\{[\s\S]*?\n\}/);
assert.ok(csvFn, "CSV serializer must exist");
const csvValue = vm.runInNewContext(csvFn[0] + "\ncsvValue");
assert.equal(csvValue("=1+1"), '"\'=1+1"');
assert.equal(csvValue("  @SUM(A1:A2)"), '"\'  @SUM(A1:A2)"');
assert.equal(csvValue('a"b,c'), '"a""b,c"');
assert.equal(csvValue(null), '""');
assert.equal(csvValue("ordinary"), '"ordinary"');
console.log("Embedded JavaScript syntax and basic UI structure: OK");
