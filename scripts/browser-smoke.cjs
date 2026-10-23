"use strict";
const assert = require("node:assert/strict");
const http = require("node:http");
const fs = require("node:fs");
const { chromium } = require("playwright");

const html = fs.readFileSync("web/index.html", "utf8");
let rejectNextChangeset = false;
let delayNextBrowse = false;
let releaseBrowse = null;
const server = http.createServer(async (req, res) => {
  res.setHeader("Content-Type", "application/json; charset=utf-8");
  if (req.url === "/") {
    res.setHeader("Content-Type", "text/html; charset=utf-8");
    res.end(html);
  } else if (req.url === "/api/schema") {
    res.end(JSON.stringify([{schema:"public",name:"demo",kind:"table"},{schema:"public",name:"other",kind:"table"}]));
  } else if (req.url.startsWith("/api/columns")) {
    res.end(JSON.stringify([{name:"id",data_type:"integer",nullable:false,primary_key:true},{name:"label",data_type:"text",nullable:true,primary_key:false}]));
  } else if (req.url === "/api/delay-browse") {
    delayNextBrowse = true;
    res.end(JSON.stringify({enabled:true}));
  } else if (req.url === "/api/release-browse") {
    if (releaseBrowse) releaseBrowse();
    res.end(JSON.stringify({released:true}));
  } else if (req.url.startsWith("/api/table-rows")) {
    if (delayNextBrowse) {
      delayNextBrowse = false;
      await new Promise(resolve=>{releaseBrowse=resolve});
      releaseBrowse = null;
    }
    const offset = Number(new URL(req.url,"http://localhost").searchParams.get("offset")||0);
    res.end(JSON.stringify({columns:["id","label"],rows:[[String(offset+1),"original"]],offset,limit:100,has_next:offset===0,stable_order:true}));
  } else if (req.url === "/api/row-change") {
    let body=""; for await(const chunk of req) body+=chunk;
    const payload=JSON.parse(body);
    assert.equal(payload.schema,"public");
    assert.equal(payload.table,"demo");
    assert.deepEqual(payload.original,["1","original"]);
    assert.deepEqual(payload.values,["1","updated"]);
    res.end(JSON.stringify({affected:1}));
  } else if (req.url === "/api/test-conflict") {
    rejectNextChangeset = true;
    res.end(JSON.stringify({enabled:true}));
  } else if (req.url === "/api/changeset") {
    if (rejectNextChangeset) {rejectNextChangeset=false;res.writeHead(409);res.end(JSON.stringify({error:"Stale row conflict"}));return;}
    let body=""; for await(const chunk of req) body+=chunk;
    const payload=JSON.parse(body);
    assert.equal(payload.schema,"public");
    assert.equal(payload.table,"demo");
    assert.equal(payload.changes.length,1);
    assert.deepEqual(payload.changes[0].original,["1","original"]);
    assert.deepEqual(payload.changes[0].values,["1","batch-value"]);
    res.end(JSON.stringify({affected:1}));
  } else if (req.url === "/api/row-insert") {
    let body=""; for await(const chunk of req) body+=chunk;
    const payload=JSON.parse(body);
    assert.equal(payload.schema,"public");
    assert.equal(payload.table,"demo");
    assert.deepEqual(payload.fields,[{column:"label",value:null}]);
    res.end(JSON.stringify({affected:1}));
  } else if (req.url === "/api/query") {
    let body=""; for await(const chunk of req) body += chunk;
    const statement=JSON.parse(body).sql;
    res.end(JSON.stringify({columns:["value"],column_types:["text"],rows:[[statement],[null],["alpha"]],affected:0,truncated:false,duration_ms:3}));
  } else {res.writeHead(404);res.end("{}");}
});
(async () => {
  await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({viewport:{width:1280,height:800},acceptDownloads:true});
    const origin="http://127.0.0.1:"+server.address().port;
    await page.goto(origin);
    await page.getByRole("heading",{name:"SQL Workbench"}).waitFor();
    const originalSQL=await page.locator("#sql").inputValue();
    await page.getByRole("button",{name:/New tab/}).click();
    await page.locator("#sql").fill("SELECT tab_two");
    await page.getByRole("tab",{name:"Query 1"}).click();
    assert.equal(await page.locator("#sql").inputValue(),originalSQL,"First tab retains SQL");
    await page.getByRole("tab",{name:"Query 2"}).click();
    assert.equal(await page.locator("#sql").inputValue(),"SELECT tab_two","Second tab retains SQL");
    await page.getByRole("button",{name:"Close Query 2"}).click();
    assert.equal(await page.locator("#sql").inputValue(),originalSQL,"Closing active tab restores first");
    await page.getByRole("button",{name:/New tab/}).click();
    await page.locator("#sql").fill("SELECT 42");
    await page.getByRole("button",{name:/Run query/}).click();
    await page.locator("#output").getByText("SELECT 42").waitFor();
    assert.equal(await page.locator("#output").getAttribute("role"),"region");
    await page.locator("#result-filter").fill("no-such-result");
    await page.getByText(/No rows match the current filter/).waitFor();
    await page.locator("#result-filter").fill("alpha");
    await page.locator("#result-filter-clear").click();
    assert.equal(await page.locator("#result-filter").inputValue(),"","Clear filter resets search");
    await page.keyboard.press("Control+Shift+f");
    assert.equal(await page.locator("#result-filter").evaluate(el=>document.activeElement===el),true,"Shortcut focuses result filter");
    await page.locator("#result-filter").fill("alpha");
    await page.locator("#output").getByText("alpha").waitFor();
    assert.equal(await page.locator("#output tbody tr").count(),1,"Filter shows one loaded row");
    await page.locator("#result-filter").fill("");
    assert.equal(await page.locator("#output tbody tr").count(),3,"Filter reset shows all rows");
    await page.locator("#theme").click();
    assert.equal(await page.locator("html").getAttribute("data-theme"),"light");
    await page.locator("#filter").fill("demo");
    await page.getByRole("button",{name:"Preview public.demo"}).click();
    await page.locator("#insert").click();
    await page.locator("#insert-dialog").waitFor({state:"visible"});
    await page.locator('#insert-fields input[data-default-field="0"]').isChecked();
    await page.locator('#insert-fields input[data-default-field="1"]').uncheck();
    await page.locator('#insert-fields input[data-null-field="1"]').check();
    await page.locator("#insert-form").getByRole("button",{name:"Insert"}).click();
    await page.locator("#insert-dialog").waitFor({state:"hidden"});
    await page.getByText(/Row inserted; table updated automatically/).waitFor();
    await page.locator("#row-edit").waitFor();
    await page.locator("#browse").click();
    await page.locator("#output .grid tbody tr").first().waitFor();
    const headers=await page.locator("#output .grid thead th").allTextContents();
    assert.deepEqual(headers.map(v=>v.trim().replace(/\\s+/g," ")),["","#","id ↕","label ↕"],"Editable grid headers match row cell order");
    const firstRow=page.locator("#output .grid tbody tr").first();
    assert.equal(await firstRow.locator("td").count(),headers.length,"Grid body and header have equal column count");
    assert.equal(await firstRow.locator("td").nth(1).innerText(),"1","Row index aligns with # header");
    await page.locator("#output .editable-cell").first().dblclick();
    await page.locator("#output .cell-editor").fill("inline-value");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    await page.locator("#output input[aria-label^='Select row']").first().check();
    await page.locator("#bulk-toggle").click();
    await page.locator("#selection-bar").waitFor({state:"visible"});
    await page.locator("#bulk-column").selectOption("1");
    await page.locator("#bulk-value").fill("batch-value");
    page.once("dialog",dialog=>dialog.accept());
    await page.locator("#bulk-apply").click();
    await page.getByText(/1 rows staged for review/).waitFor();
    await page.locator("#changes-save").click();
    await page.locator("#changes-review").waitFor({state:"visible"});
    await page.locator("#changes-review-summary").getByText(/1 row\(s\), 1 changed cell\(s\)/).waitFor();
    await page.locator("#changes-review").getByText("batch-value").waitFor();
    await page.locator("#changes-review-confirm").click();
    await page.locator("#changes-bar").waitFor({state:"hidden"});
    assert.match(await page.locator("#selection-bar").textContent(),/saved atomically/);

    await page.getByRole("button",{name:"Browse table rows"}).click();
    await page.locator("#server-page-info").getByText(/Database rows 1/).waitFor();
    await page.locator("#browse").click();
    await page.locator("#output input[aria-label^='Select row']").first().check();
    await page.locator("#row-edit").click();
    await page.locator("#row-dialog").waitFor({state:"visible"});
    await page.locator('#row-fields input[data-column="1"]').fill("updated");
    await page.getByRole("button",{name:"Save changes"}).click();
    await page.locator("#row-dialog").waitFor({state:"hidden"});
    await page.getByText(/Row updated; changes visible immediately/).waitFor();
    await page.route("**/api/row-change",async route=>route.fulfill({status:409,contentType:"application/json",body:JSON.stringify({error:"Row changed or no longer exists"})}));
    await page.locator("#output input[aria-label^='Select row']").first().check();
    await page.locator("#row-edit").click();
    await page.locator('#row-fields input[data-column="1"]').fill("unsaved-draft");
    await page.getByRole("button",{name:"Save changes"}).click();
    await page.getByText(/Your edits are still in the editor/).waitFor();
    assert.equal(await page.locator("#row-dialog").isVisible(),true,"409 retains editor");
    assert.equal(await page.locator('#row-fields input[data-column="1"]').inputValue(),"unsaved-draft");
    await page.locator("#row-cancel").click();
    await page.unroute("**/api/row-change");
    await page.locator("#server-next").click();
    await page.locator("#server-page-info").getByText(/Database rows 101/).waitFor();
    await page.evaluate(()=>fetch("/api/test-conflict"));
    await page.locator("#browse").click();
    await page.locator("#output .editable-cell").first().dblclick();
    await page.locator("#output .cell-editor").fill("retry-value");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-save").click();
    await page.locator("#changes-review-confirm").click();
    await page.getByText(/No staged rows committed/).waitFor();
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    assert.equal(await page.locator("#output .pending-cell").count(),1,"Conflict retains draft");
    page.once("dialog",dialog=>dialog.accept());
    await page.getByRole("button",{name:"Run query",exact:false}).click();
    await page.locator("#server-pager").waitFor({state:"hidden"});
    assert.equal(await page.locator("#server-pager").isVisible(),false,"Free SQL hides server pager");
    await page.evaluate(()=>fetch("/api/delay-browse"));
    await page.locator("#browse").click();
    await page.locator("#filter").fill("");
    await page.getByRole("button",{name:"Preview public.other"}).click();
    await page.locator("#selected").getByText("public.other").waitFor();
    await page.evaluate(()=>fetch("/api/release-browse"));
    await page.waitForTimeout(150);
    assert.equal(await page.locator("#output .grid").count(),0,"Stale table A must not appear after switching to B");
    assert.equal(await page.locator("#selected").innerText(),"public.other");
    console.log("Chromium workbench UI smoke tests passed");
  } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>server.close());
