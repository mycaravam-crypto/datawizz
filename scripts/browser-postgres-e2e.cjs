"use strict";
const assert = require("node:assert/strict");
const { chromium } = require("playwright");

(async () => {
  const browser = await chromium.launch({headless:true});
  try {
    const page = await browser.newPage({viewport:{width:1280,height:800}});
    const denied=await page.request.get("http://127.0.0.1:3188/api/connections");
    assert.equal(denied.status(),401,"Unauthenticated API requests must be rejected");
    const navigation = [];
    page.on("framenavigated",frame=>{if(frame===page.mainFrame())navigation.push(frame.url())});
    await page.goto("http://127.0.0.1:3188/");
    await page.getByRole("heading",{name:"SQL Workbench"}).waitFor();
    await page.locator("#auth-area").waitFor({state:"visible"});
    await page.locator("#access-token").fill("0123456789abcdefghijklmnopqrstuvwxyz");
    await page.locator("#auth-unlock").click();
    await page.locator("#auth-area").waitFor({state:"hidden"});

    // Administration opens in a separate explorer/detail workspace.
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-refresh").click();
    await page.locator("#admin-output .grid").waitFor();
    await page.locator("#admin-output").getByText("datawizz_test",{exact:true}).first().waitFor();
    const adminResponse=await page.request.get("http://127.0.0.1:3188/api/admin",{headers:{Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"}});
    assert.equal(adminResponse.status(),200);
    const admin=await adminResponse.json();
    assert(admin.roles.some(r=>r.name==="datawizz_test"));
    assert(admin.databases.some(d=>d.name==="datawizz_test"));
    assert(admin.extensions.length>0);
    const adminJson=JSON.stringify(admin);
    for(const hidden of ['"query"','"rolpassword"','"client_addr"'])assert(!adminJson.includes(hidden));

    assert(admin.schemas.some(s=>s.name==="public"));
    assert(admin.relations.some(r=>r.schema==="public"&&r.name==="datawizz_browser_e2e"));
    assert(admin.indexes.some(i=>i.table==="datawizz_browser_e2e"&&i.valid
      &&i.definition.includes("CREATE UNIQUE INDEX")));
    assert(admin.constraints.some(c=>c.table==="datawizz_browser_e2e"&&c.kind==="p"));
    assert(admin.table_stats.some(t=>t.table==="datawizz_browser_e2e"));
    assert(admin.settings.some(s=>s.name==="max_connections"));
    assert(Array.isArray(admin.locks)&&Array.isArray(admin.privileges)&&Array.isArray(admin.routines));
    await page.locator("#admin-nav-relations").click();
    await page.locator("#admin-section-search").fill("datawizz_browser_e2e");
    await page.locator("#admin-output tbody tr").first().click();
    await page.locator("#admin-detail").getByText("datawizz_browser_e2e").waitFor();
    await page.locator("#admin-nav-indexes").click();
    await page.locator("#admin-section-search").fill("datawizz_browser_e2e");
    await page.locator("#admin-output tbody tr").first().click();
    assert.match(await page.locator("#admin-detail .admin-ddl-preview").textContent(),/CREATE UNIQUE INDEX/);
    await page.locator("#admin-sql-nav").click();
    await page.locator("#admin-task").selectOption("view-create");
    await page.locator("#admin-field-0").fill("public");
    await page.locator("#admin-field-1").fill("datawizz_preview_only");
    await page.locator("#admin-field-2").fill("datawizz_browser_e2e");
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),
      'CREATE VIEW "public"."datawizz_preview_only" AS SELECT * FROM "public"."datawizz_browser_e2e";');
    // No view is created by generating the statement.
    const viewCheck=await page.request.get("http://127.0.0.1:3188/api/schema",
      {headers:{Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"}});
    assert(!(await viewCheck.json()).some(obj=>obj.name==="datawizz_preview_only"));
    await page.locator("#admin-task").selectOption("table-grant");
    await page.locator("#admin-field-0").fill("public");
    await page.locator("#admin-field-1").fill("datawizz_browser_e2e");
    await page.locator("#admin-field-2").fill("PUBLIC");
    await page.locator("#admin-generate").click();
    assert.equal(await page.locator("#admin-sql-preview").textContent(),'GRANT SELECT ON TABLE "public"."datawizz_browser_e2e" TO PUBLIC;');
    await page.locator("#admin-open-sql").click();
    assert.equal(await page.locator("#workspace-tab-sql").getAttribute("aria-selected"),"true");
    assert.match(await page.locator("#sql").inputValue(),/^GRANT SELECT ON TABLE/);
    // The generated statement is never run automatically; the existing SQL tab is preserved.


    // Import preview maps CSV headers to real PostgreSQL columns, but never writes.
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-csv-nav").click();
    await page.locator("#admin-csv-table").fill("datawizz_browser_e2e");
    await page.locator("#admin-csv-file").setInputFiles({
      name:"example.csv",mimeType:"text/csv",buffer:Buffer.from("id,label\n99,dry-run\n")
    });
    await page.locator("#admin-csv-preview").click();
    await page.waitForFunction(() => document.getElementById("admin-csv-status").textContent.includes("row(s) mapped"));
    await page.locator("#admin-csv-open").waitFor({state:"visible"});
    assert.equal(await page.locator("#admin-csv-open").isDisabled(),false);
    assert.match(await page.locator("#admin-csv-sql").textContent(),/INSERT INTO "public"."datawizz_browser_e2e"/);
    await page.locator("#admin-csv-open").click();
    assert.equal(await page.locator("#workspace-tab-sql").getAttribute("aria-selected"),"true");
    const checkImport=await page.request.get("http://127.0.0.1:3188/api/table-rows?schema=public&table=datawizz_browser_e2e&offset=0&limit=20",
      {headers:{Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"}});
    assert(!(await checkImport.json()).rows.some(row=>row.includes("dry-run")),"Preview must not modify data");

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
    // Saving hides the staged-changes bar before the follow-up browse completes.
    // Avoid clicking Refresh while the application is still busy.
    await page.waitForFunction(() => !busy);
    await page.locator("#output").getByText("after-inline",{exact:true}).waitFor();
    assert.equal(navigation.length,navigationCount,"Inline edit must not reload the page");

    // The persisted value also survives a fresh table fetch.
    await page.locator("#browse").click();
    await page.waitForFunction(() => !busy);
    // Explicit refresh must return to read-only; opt in before selecting rows.
    await page.locator("#edit-mode").waitFor({state:"visible"});
    if (await page.locator("#edit-mode").getAttribute("aria-pressed") !== "true") {
      await page.locator("#edit-mode").click();
    }
    await page.locator("#output input[aria-label^='Select row']").first().waitFor();
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
    const response = await page.request.get("http://127.0.0.1:3188/api/table-rows?schema=public&table=datawizz_browser_e2e&offset=0&limit=20",{headers:{Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"}});
    assert.equal(response.status(),200);
    const rows = await response.json();
    assert.equal(rows.rows[0][1],"after-dialog");

    // Real PostgreSQL EXPLAIN result is displayed as a navigable, text-only plan tree.
    await page.locator("#sql").fill("EXPLAIN (FORMAT JSON) SELECT * FROM public.datawizz_browser_e2e;");
    await page.locator("#run").click();
    await page.locator("#output .plan-view .plan-node").first().waitFor();
    assert.match(await page.locator("#output .plan-view").textContent(),/Scan/);
    assert.equal(await page.locator("#output img").count(),0);
    // History is local session state; reopening cannot re-execute SQL on the DB.
    await page.locator("#sql-productivity").evaluate(el=>el.open=true);
    await page.locator("#sql-history-search").fill("EXPLAIN (FORMAT JSON)");
    await page.locator("#sql-history-list [role=listitem]").first().waitFor();
    let replayRequests=0;
    await page.route("**/api/query",route=>{replayRequests++;return route.continue()});
    await page.locator("#sql-history-list [role=listitem]").first().getByRole("button",{name:"Open"}).click();
    assert.match(await page.locator("#sql").inputValue(),/^EXPLAIN \(FORMAT JSON\)/);
    assert.equal(replayRequests,0,"Opening history cannot execute SQL");
    assert.equal(await page.evaluate(()=>localStorage.getItem("datawizz.saved-sql.v1")),null,
      "Query history must not be saved to persistent browser storage");
    await page.unroute("**/api/query");
    console.log("Real PostgreSQL browser editing and EXPLAIN passed");
  } finally {await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
