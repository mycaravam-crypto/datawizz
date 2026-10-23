"use strict";
const assert = require("node:assert/strict");
const { chromium } = require("playwright");

(async () => {
  const browser = await chromium.launch({headless:true});
  try {
    const page = await browser.newPage({viewport:{width:1280,height:800}});
    const navigation = [];
    page.on("framenavigated",frame=>{if(frame===page.mainFrame())navigation.push(frame.url())});
    await page.goto("http://127.0.0.1:3188/");
    await page.getByRole("heading",{name:"SQL Workbench"}).waitFor();
    await page.locator("#filter").fill("datawizz_browser_e2e");
    await page.getByRole("button",{name:"Preview public.datawizz_browser_e2e"}).click();
    // Column metadata loads asynchronously; row editing is enabled only after it arrives.
    await page.locator("#columns .column-item").first().waitFor();
    await page.locator("#browse").click();
    await page.waitForFunction(() => !busy);
    await page.locator("#edit-mode").click();
    await page.locator("#output").getByText("before",{exact:true}).waitFor();
    const navigationCount = navigation.length;

    // Inline editing stages, then commits through the real atomic PostgreSQL endpoint.
    await page.locator("#output .editable-cell").first().dblclick();
    await page.locator("#output .cell-editor").fill("after-inline");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    await page.locator("#changes-save").click();
    await page.locator("#changes-review").waitFor({state:"visible"});
    await page.locator("#changes-review-confirm").click();
    await page.locator("#changes-bar").waitFor({state:"hidden"});
    await page.locator("#output").getByText("after-inline",{exact:true}).waitFor();
    assert.equal(navigation.length,navigationCount,"Inline edit must not reload the page");

    // The persisted value also survives a fresh table fetch.
    await page.locator("#browse").click();
    await page.waitForFunction(() => !busy);
    await page.locator("#edit-mode").click();
    await page.locator("#output").getByText("after-inline",{exact:true}).waitFor();
    // The existing grid can remain visible while the fresh request runs.
    // Wait for the application to finish browsing before attempting row editing.
    await page.waitForFunction(() => !busy);

    // Editing through the row dialog must update the current grid automatically.
    await page.locator("#output input[aria-label^='Select row']").first().check();
    await page.locator("#row-edit").click();
    await page.locator("#row-dialog").waitFor({state:"visible"});
    await page.locator('#row-fields input[data-column="1"]').fill("after-dialog");
    await page.getByRole("button",{name:"Save changes"}).click();
    await page.locator("#output").getByText("after-dialog",{exact:true}).waitFor();
    assert.equal(navigation.length,navigationCount,"Row dialog edit must not reload the page");

    // Persisted writes are visible from a separate server read, not only DOM state.
    const response = await page.request.get("http://127.0.0.1:3188/api/table-rows?schema=public&table=datawizz_browser_e2e&offset=0&limit=20");
    assert.equal(response.status(),200);
    const rows = await response.json();
    assert.equal(rows.rows[0][1],"after-dialog");
    console.log("Real PostgreSQL browser editing without page reload passed");
  } finally {await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
