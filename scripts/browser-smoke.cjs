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
  } else if (req.url === "/api/admin") {
    res.end(JSON.stringify({
      databases:[{name:"test",owner:"test",allow_connections:true}],
      roles:[{name:"<img src=x onerror=alert(1)>",can_login:false,superuser:false,create_db:false,create_role:false}],
      memberships:[],sessions:[],extensions:[],schemas:[],
      relations:[{schema:"public",name:"demo_view",kind:"view",owner:"test",estimated_rows:0,size_bytes:0,definition:"SELECT id FROM public.demo"}],
      indexes:[{schema:"public",table:"demo",name:"demo_idx",valid:true,scans:0,definition:"CREATE INDEX demo_idx ON public.demo USING btree (id)"}],
      constraints:[],table_stats:[],locks:[],settings:[],privileges:[],routines:[]
    }));
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
    if(/^EXPLAIN\s*\(FORMAT JSON\)/i.test(statement)){
      const plan=[{Plan:{"Node Type":"Aggregate","Total Cost":2.3,"Plan Rows":1,
        Plans:[{"Node Type":"Seq Scan","Relation Name":"demo","Total Cost":1.5}]}}];
      res.end(JSON.stringify({columns:["QUERY PLAN"],column_types:["text"],
        rows:[[JSON.stringify(plan)]],affected:0,truncated:false,duration_ms:2}));
    }else{
      res.end(JSON.stringify({columns:["value"],column_types:["text"],rows:[[statement],[null],["alpha"]],affected:0,truncated:false,duration_ms:3}));
    }
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
    await page.waitForFunction(() => !busy);
    await page.locator("#edit-mode").click();
    await page.locator("#output .grid tbody tr").first().waitFor();
    // A previously rendered grid can be visible while the new browse request is still busy.
    await page.waitForFunction(() => !busy);
    const headers=await page.locator("#output .grid thead th").allTextContents();
    assert.deepEqual(headers.map(v=>v.trim().replace(/\\s+/g," ")),["","#","id ↕","label ↕"],"Editable grid headers match row cell order");
    const firstRow=page.locator("#output .grid tbody tr").first();
    assert.equal(await firstRow.locator("td").count(),headers.length,"Grid body and header have equal column count");
    assert.equal(await firstRow.locator("td").nth(1).innerText(),"1","Row index aligns with # header");
    const cell=page.locator("#output .editable-cell").first();
    await cell.focus();
    await cell.press("F2");
    await page.locator("#output .cell-editor").fill("first-draft");
    await page.locator("#output .cell-editor").press("Tab");
    assert.equal(await page.locator("#output .editable-cell:focus").count(),1,"Tab returns keyboard focus to the grid");
    assert.match(await page.locator("#grid-keyboard-hint").innerText(),/Tab next/);
    await page.locator("#output .editable-cell").first().focus();
    await page.locator("#output .editable-cell").first().press("F2");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    await page.locator("#changes-undo").click();
    await page.locator("#changes-bar").waitFor({state:"hidden"});
    await page.locator("#changes-redo").click();
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    await page.locator("#changes-undo").click();
    await page.locator("#changes-bar").waitFor({state:"hidden"});
    await cell.focus();
    await cell.press("Enter");
    await page.locator("#output .cell-editor").fill("inline-value");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    assert.equal(await page.locator("#changes-undo").isDisabled(),false);
    // A focused editable cell must support directional keyboard navigation.
    await page.locator("#output .editable-cell").first().focus();
    await page.locator("#output .editable-cell").first().press("ArrowDown");
    assert.equal(await page.locator("#output .editable-cell:focus").count(),1);

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

    await page.locator("#browse-table").evaluate(button=>button.click());
    await page.locator("#server-page-info").getByText(/Database rows 1/).waitFor();
    await page.locator("#browse").click();
    await page.waitForFunction(() => !busy);
    await page.locator("#edit-mode").click();
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
    await page.waitForFunction(() => !busy);
    await page.locator("#edit-mode").click();
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

    // DataGrip-style workspace: one visible grid at a time, explorer on the left.
    await page.locator("#workspace-tab-admin").click();
    assert.equal(await page.locator("#sql-workspace").isVisible(),false);
    assert.equal(await page.locator("#admin-workspace").isVisible(),true);
    await page.locator("#admin-refresh").click();
    await page.locator("#admin-output .grid").waitFor();
    assert.equal(await page.locator("#admin-nav .admin-tree-button").count(),16);
    assert.equal(await page.locator("#admin-output .grid").count(),1,"Only the selected catalog must be visible");
    await page.locator("#admin-nav-roles").click();
    await page.locator("#admin-output").getByText("<img src=x onerror=alert(1)>").waitFor();
    assert.equal(await page.locator("#admin-output img").count(),0,"Catalog data must not create HTML nodes");
    await page.locator("#admin-output tbody tr").first().click();
    assert.equal(await page.locator("#admin-detail").isVisible(),true,"Selected row must open details");
    await page.locator("#admin-section-search").fill("no-matching-role");
    await page.locator("#admin-output").getByText("No matching records.").waitFor();
    await page.locator("#admin-section-search").fill("");
    await page.locator("#admin-nav-indexes").click();
    assert.equal(await page.locator("#admin-view-title").textContent(),"Indexes");
    assert.equal(await page.locator("#admin-output .grid").count(),1);
    await page.locator("#admin-output tbody tr").first().click();
    assert.match(await page.locator("#admin-detail .admin-ddl-preview").textContent(),/CREATE INDEX demo_idx/);
    let previewPosted=false;
    await page.route("**/api/query",route=>{previewPosted=true;return route.continue()});
    await page.locator("#admin-detail").getByRole("button",{name:"Open DDL in SQL console"}).click();
    assert.equal(previewPosted,false,"Opening DDL must not execute SQL");
    assert.equal(await page.locator("#workspace-tab-sql").getAttribute("aria-selected"),"true");
    assert.match(await page.locator("#sql").inputValue(),/^CREATE INDEX demo_idx/);
    await page.unroute("**/api/query");
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-nav-relations").click();
    await page.locator("#admin-output tbody tr").first().click();
    assert.match(await page.locator("#admin-detail .admin-ddl-preview").textContent(),/^CREATE VIEW "public"."demo_view" AS/);
    await page.setViewportSize({width:700,height:800});
    const compactNav=await page.locator(".admin-explorer").boundingBox();
    const compactStage=await page.locator(".admin-stage").boundingBox();
    assert(compactNav.y<compactStage.y,"Narrow admin workspace stacks navigation above details");
    await page.setViewportSize({width:1280,height:800});
    const wideNav=await page.locator(".admin-explorer").boundingBox();
    const wideStage=await page.locator(".admin-stage").boundingBox();
    assert(wideNav.x<wideStage.x,"Wide admin workspace places explorer to the left");
    await page.locator("#admin-sql-nav").click();
    assert.equal(await page.locator("#admin-sql-view").isVisible(),true);
    let autoExecuted=0;
    await page.route("**/api/query",route=>{autoExecuted++;return route.continue()});
    await page.locator("#admin-task").selectOption("table-grant");
    await page.locator("#admin-field-0").fill("public");
    await page.locator("#admin-field-1").fill("orders");
    await page.locator("#admin-field-2").fill("PUBLIC");
    await page.locator("#admin-privilege").selectOption("SELECT");
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),'GRANT SELECT ON TABLE "public"."orders" TO PUBLIC;');
    await page.locator("#admin-open-sql").click();
    assert.equal(await page.locator("#workspace-tab-sql").getAttribute("aria-selected"),"true","Generated SQL opens SQL workspace");
    assert.equal(await page.locator("#sql").inputValue(),'GRANT SELECT ON TABLE "public"."orders" TO PUBLIC;');
    assert.equal(autoExecuted,0,"Preparing SQL must not call the query endpoint");
    await page.getByRole("tab",{name:"Query 1"}).click();
    assert.notEqual(await page.locator("#sql").inputValue(),'GRANT SELECT ON TABLE "public"."orders" TO PUBLIC;');
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-task").selectOption("role-create");
    await page.locator("#admin-field-0").fill('bad"; DROP ROLE admin; --');
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),'CREATE ROLE "bad""; DROP ROLE admin; --" NOLOGIN;');
    await page.locator("#admin-task").selectOption("column-add");
    await page.locator("#admin-field-0").fill("public");
    await page.locator("#admin-field-1").fill("orders");
    await page.locator("#admin-field-2").fill('new"field');
    await page.locator("#admin-field-3").fill("JSONB");
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),
      'ALTER TABLE "public"."orders" ADD COLUMN "new""field" JSONB;');
    await page.locator("#admin-field-3").fill("text); DROP TABLE orders; --");
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-open-sql").isDisabled(),true,
      "SQL type inputs must be restricted to an allowlist");
    await page.locator("#admin-task").selectOption("index-create");
    for(const [i,value] of ["public","orders","orders_customer_idx","customer_id"].entries())
      await page.locator("#admin-field-"+i).fill(value);
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),
      'CREATE INDEX "orders_customer_idx" ON "public"."orders" ("customer_id");');
    await page.locator("#admin-task").selectOption("foreignkey-add");
    for(const [i,value] of ["public","orders","customer_id","customers","id"].entries())
      await page.locator("#admin-field-"+i).fill(value);
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),
      'ALTER TABLE "public"."orders" ADD FOREIGN KEY ("customer_id") REFERENCES "public"."customers" ("id");');
    await page.locator("#admin-task").selectOption("column-set-default");
    for(const [i,value] of ["public","orders","label","O'Reilly'; DROP TABLE orders; --"].entries())
      await page.locator("#admin-field-"+i).fill(value);
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),
      'ALTER TABLE "public"."orders" ALTER COLUMN "label" SET DEFAULT \'O\'\'Reilly\'\'; DROP TABLE orders; --\';');
    await page.locator("#admin-task").selectOption("table-comment");
    for(const [i,value] of ["public","orders","Pilot's table"].entries())
      await page.locator("#admin-field-"+i).fill(value);
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),
      'COMMENT ON TABLE "public"."orders" IS \'Pilot\'\'s table\';');
    await page.locator("#admin-task").selectOption("session-terminate");
    await page.locator("#admin-field-0").fill("12); DROP TABLE x;");
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-open-sql").isDisabled(),true,"Unsafe PID must be rejected");
    assert.match(await page.locator("#admin-sql-message").textContent(),/PID must be/);
    assert.equal(autoExecuted,0);
    await page.unroute("**/api/query");
    // CSV preview and EXPLAIN must never execute the generated statement.
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-csv-nav").click();
    await page.locator("#admin-csv-table").fill("demo");
    await page.locator("#admin-csv-file").setInputFiles({
      name:"rows.csv",mimeType:"text/csv",
      buffer:Buffer.from('id,label\n123,"Robert); DROP TABLE secrets; --"\n124,"quoted ""label"""\n')
    });
    let csvExecutionCount=0;
    await page.route("**/api/query",route=>{csvExecutionCount++;return route.continue()});
    await page.locator("#admin-csv-preview").click();
    await page.waitForFunction(() => document.getElementById("admin-csv-status").textContent.includes("row(s) mapped"));
    assert.equal(await page.locator("#admin-csv-open").isDisabled(),false,
      "CSV preview rejected: "+await page.locator("#admin-csv-status").textContent());
    const preview=await page.locator("#admin-csv-sql").textContent();
    assert(preview.includes('INSERT INTO "public"."demo" ("id", "label") VALUES'));
    assert(preview.includes('$dw$Robert); DROP TABLE secrets; --$dw$'));
    assert(preview.includes('$dw$quoted "label"$dw$'));
    await page.locator("#admin-csv-open").click();
    assert.equal(await page.locator("#workspace-tab-sql").getAttribute("aria-selected"),"true");
    assert.equal(csvExecutionCount,0);
    await page.locator("#explain").click();
    assert((await page.locator("#sql").inputValue()).startsWith("EXPLAIN (FORMAT JSON) INSERT INTO "));
    assert.equal(csvExecutionCount,0);
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-csv-nav").click();
    await page.locator("#admin-csv-file").setInputFiles({
      name:"invalid.csv",mimeType:"text/csv",buffer:Buffer.from("id,label\n1,ok\n2\n")
    });
    await page.locator("#admin-csv-preview").click();
    await page.waitForFunction(() => document.getElementById("admin-csv-status").textContent.includes("different column count"));
    assert.equal(await page.locator("#admin-csv-open").isDisabled(),true,"Invalid CSV row width must be rejected");
    assert.match(await page.locator("#admin-csv-status").textContent(),/different column count/);
    await page.unroute("**/api/query");
    await page.locator("#workspace-tab-sql").click();
    await page.locator("#sql").fill("EXPLAIN (FORMAT JSON) SELECT * FROM public.demo;");
    await page.locator("#run").click();
    await page.locator("#output .plan-view .plan-node").first().waitFor();
    assert.deepEqual(await page.locator("#output .plan-view .plan-node").allTextContents(),
      ["Aggregate","Seq Scan · demo"]);
    assert.match(await page.locator("#output .plan-view").textContent(),/Total Cost: 2.3/);
    assert.equal(await page.locator("#output img").count(),0,"Plan text is not HTML");
    console.log("Chromium workbench UI smoke tests passed");
  } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>server.close());
