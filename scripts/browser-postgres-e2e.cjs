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
    await page.goto("http://127.0.0.1:3188/?basic-editor=1");
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
    // Server-side search is independent from the currently loaded 100-row page.
    await page.locator("#filter").fill("datawizz_browser_e2e");
    await page.getByRole("button",{name:"Preview public.datawizz_browser_e2e"}).click();
    await page.waitForFunction(() => document.querySelectorAll("#table-filter-column option").length>1);
    await page.locator("#table-filter-column").selectOption("label");
    await page.locator("#table-filter-value").fill("after-dialog");
    await page.locator("#table-sort-column").selectOption("label");
    await page.locator("#table-sort-dir").selectOption("desc");
    await page.locator("#table-apply").click();
    await page.waitForFunction(() => !busy && tableMode);
    await page.locator("#output").getByText("after-dialog",{exact:true}).waitFor();
    assert.equal(await page.locator("#output .grid tbody tr").count(),1);
    // PostgreSQL E2E: a second catalog-validated AND filter is applied on the server.
    await page.locator("#table-filter2-column").selectOption("id");
    await page.locator("#table-filter2-value").fill("999");
    await page.locator("#table-apply").click();
    await page.waitForFunction(()=>!busy && tableMode);
    assert.equal(await page.locator("#output .grid tbody tr").count(),0);
    await page.locator("#table-filter2-value").fill("1");
    const compoundResponse=page.waitForResponse(r=>r.url().includes("/api/table-rows?")&&
      r.url().includes("filter2=1"));
    await page.locator("#table-apply").click();
    const compoundQuery=new URL((await compoundResponse).url()).searchParams;
    assert.equal(compoundQuery.get("filter2_column"),"id");
    await page.locator("#output").getByText("after-dialog",{exact:true}).waitFor();
    assert.equal(await page.locator("#output .grid tbody tr").count(),1);
    const tokenHeaders={Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"};
    const url=(changes)=>"http://127.0.0.1:3188/api/table-rows?"+
      new URLSearchParams({schema:"public",table:"datawizz_browser_e2e",limit:"20",offset:"0",...changes});
    const filteredRows=await page.request.get(url({filter_column:"label",filter:"after-dialog",sort_column:"label",sort_dir:"desc"}),{headers:tokenHeaders});
    assert.equal(filteredRows.status(),200);
    assert.equal((await filteredRows.json()).rows.length,1);
    const compoundRows=await page.request.get(url({
      filter_column:"label",filter:"after-dialog",
      filter2_column:"id",filter2:"999",sort_column:"label",sort_dir:"desc"
    }),{headers:tokenHeaders});
    assert.equal(compoundRows.status(),200);
    assert.equal((await compoundRows.json()).rows.length,0);
    const invalidSecond=await page.request.get(url({
      filter_column:"label",filter:"after-dialog",
      filter2_column:"id) OR TRUE --",filter2:"1"
    }),{headers:tokenHeaders});
    assert.equal(invalidSecond.status(),400);
    const missingSecondColumn=await page.request.get(url({
      filter_column:"label",filter:"after-dialog",filter2:"1"
    }),{headers:tokenHeaders});
    assert.equal(missingSecondColumn.status(),400);
    const badColumn=await page.request.get(url({filter_column:"label) OR true--",filter:"after-dialog"}),{headers:tokenHeaders});
    assert.equal(badColumn.status(),400);
    const badSort=await page.request.get(url({sort_column:"label",sort_dir:"desc; DROP TABLE x"}),{headers:tokenHeaders});
    assert.equal(badSort.status(),400);
    assert.equal(await page.locator("#table-export").isDisabled(),false,
      "Export disabled: "+await page.locator("#message").textContent());
    const downloading=page.waitForEvent("download",{timeout:8000}).catch(()=>null);
    await page.locator("#table-export").click();
    await page.waitForFunction(() => !busy);
    const exportMessage=await page.locator("#message").textContent();
    assert.match(exportMessage,/Exported database CSV from a consistent snapshot/,
      "Export did not finish: "+exportMessage);
    const download=await downloading;
    assert(download,"Export reported success but no browser download was emitted: "+exportMessage);
    assert.equal(download.suggestedFilename(),"datawizz-filtered-table.csv");
    const fs=require("node:fs");
    const downloadedCsv=fs.readFileSync(await download.path(),"utf8");
    assert(downloadedCsv.includes("after-dialog"));
    assert(!downloadedCsv.includes("before"),"Snapshot CSV must match the active filters");
    // API download is authenticated, rejects injected identifiers and returns CSV.
    const exportApi="http://127.0.0.1:3188/api/table-export?";
    const exportParams=new URLSearchParams({
      schema:"public",table:"datawizz_browser_e2e",
      filter_column:"label",filter:"after-dialog",
      filter2_column:"id",filter2:"1",sort_column:"id",sort_dir:"asc"
    });
    const serverExport=await page.request.get(exportApi+exportParams,{headers:tokenHeaders});
    assert.equal(serverExport.status(),200);
    assert.match(serverExport.headers()["content-type"],/^text\/csv/);
    assert((await serverExport.text()).includes("after-dialog"));
    const deniedExport=await page.request.get(exportApi+exportParams);
    assert.equal(deniedExport.status(),401);
    const injectedExport=await page.request.get(exportApi+new URLSearchParams({
      schema:"public",table:"datawizz_browser_e2e",
      filter_column:"id); DROP TABLE x; --",filter:"1"
    }),{headers:tokenHeaders});
    assert.equal(injectedExport.status(),400);
    await page.locator("#table-reset").click();
    await page.locator("#output").getByText("after-dialog",{exact:true}).waitFor();
    // Two physical databases: routes are isolated even with the same bearer token.
    const headers={Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"};
    const base="http://127.0.0.1:3188";
    const profiles=await page.request.get(base+"/api/connections",{headers});
    assert.equal(profiles.status(),200);
    const profileList=await profiles.json();
    assert.deepEqual(profileList.map(p=>p.id),["default","sandbox"]);
    assert(!JSON.stringify(profileList).includes("postgres://"));
    assert.equal(profileList[1].read_only,true);
    const sandbox=await page.request.get(base+"/profiles/sandbox/api/connection-status",{headers});
    assert.equal(sandbox.status(),200);
    assert.equal((await sandbox.json()).database,"datawizz_profile_test");
    const sandboxExport=await page.request.get(base+"/profiles/sandbox/api/table-export?"+
      new URLSearchParams({schema:"public",table:"profile_only"}),{headers});
    assert.equal(sandboxExport.status(),200,"Read-only profiles may export");
    const sandboxCsv=await sandboxExport.text();
    assert(sandboxCsv.includes("sandbox-only"));
    assert(!sandboxCsv.includes("after-dialog"),"Export must not cross database profiles");
    const otherObjects=await page.request.get(base+"/profiles/sandbox/api/schema",{headers});
    assert.equal(otherObjects.status(),200);
    const inSandbox=await otherObjects.json();
    assert(inSandbox.some(o=>o.name==="profile_only"));
    assert(!inSandbox.some(o=>o.name==="datawizz_browser_e2e"));
    const defaultObjects=await page.request.get(base+"/api/schema",{headers});
    assert(!(await defaultObjects.json()).some(o=>o.name==="profile_only"));
    const missing=await page.request.get(base+"/profiles/other/api/schema",{headers});
    assert.equal(missing.status(),404);
    const deniedInsert=await page.request.post(base+"/profiles/sandbox/api/row-insert",
      {headers,data:{schema:"public",table:"profile_only",fields:[]}});
    assert.equal(deniedInsert.status(),403);
    const deniedWrite=await page.request.post(base+"/profiles/sandbox/api/query",
      {headers,data:{sql:"INSERT INTO public.profile_only VALUES (2, 'forbidden')"}});
    assert.equal(deniedWrite.status(),400);
    assert.match((await deniedWrite.json()).error,/25006/);
    const readResult=await page.request.post(base+"/profiles/sandbox/api/query",
      {headers,data:{sql:"SELECT label FROM public.profile_only WHERE id=1"}});
    assert.equal(readResult.status(),200);
    assert.equal((await readResult.json()).rows[0][0],"sandbox-only");
    const fk=await page.request.get(base+"/api/relationships",{headers});
    assert.equal(fk.status(),200);
    assert((await fk.json()).some(e=>e.from_table==="datawizz_er_child" &&
      e.to_table==="datawizz_browser_e2e"));

    // Profile switching discards the in-memory editor, history and metadata.
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-er-nav").click();
    await page.locator("#er-accessible tbody tr").first().waitFor();
    assert.match(await page.locator("#er-status").textContent(),/relationships/);
    // Table links navigate to authorized PostgreSQL metadata; never run SQL.
    const startingWidth=Number(await page.locator("#er-diagram svg").getAttribute("width"));
    await page.locator("#er-zoom-in").click();
    assert(Number(await page.locator("#er-diagram svg").getAttribute("width"))>startingWidth);
    await page.locator("#er-zoom-reset").click();
    assert.equal(Number(await page.locator("#er-diagram svg").getAttribute("width")),startingWidth);
    let erExecutedSql=false;
    await page.route("**/api/query",route=>{erExecutedSql=true;return route.continue()});
    await page.locator('#er-accessible button[aria-label="Inspect source relation public.datawizz_er_child"]').click();
    assert.equal(await page.locator("#admin-view-title").textContent(),"Relations");
    assert.match(await page.locator("#admin-detail").textContent(),/datawizz_er_child/);
    assert.equal(erExecutedSql,false,"ER navigation must not execute SQL");
    await page.unroute("**/api/query");
    await page.locator("#workspace-tab-sql").click();
    await page.locator("#sql").fill("SELECT 'must not cross profile boundaries'");
    // Preserve a staged database row as well as the unsaved SQL draft on failed switches.
    await page.waitForFunction(() => !busy);
    if(await page.locator("#edit-mode").getAttribute("aria-pressed")!=="true"){
      await page.locator("#edit-mode").click();
    }
    await page.locator("#output .editable-cell").first().dblclick();
    await page.locator("#output .cell-editor").fill("unsaved-switch-draft");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    const beforeFailureNavigations=navigation.length;
    const originalStored=await page.evaluate(()=>sessionStorage.getItem("datawizz.profile-id.v1"));
    // A lost database must not trigger a reload, discard edits or surface DB secrets.
    await page.route("**/profiles/sandbox/api/connection-status",route=>route.fulfill({
      status:502,contentType:"application/json",
      body:JSON.stringify({error:"postgres://secret-user:secret-password@host"})
    }));
    await page.locator("#connection-chooser").selectOption("sandbox");
    await page.locator("#message").getByText(/Profile switch aborted; workspace preserved/).waitFor();
    assert.equal(await page.locator("#connection-chooser").inputValue(),"default");
    assert.equal(navigation.length,beforeFailureNavigations,"Unreachable profile must not navigate");
    assert.equal(await page.evaluate(()=>sessionStorage.getItem("datawizz.profile-id.v1")),originalStored);
    assert.equal(await page.locator("#sql").inputValue(),"SELECT 'must not cross profile boundaries'");
    assert.equal(await page.locator("#changes-count").textContent(),"1 pending row");
    assert(!String(await page.locator("#message").textContent()).includes("secret-password"),
      "Remote connection diagnostics must not leak secrets");
    await page.unroute("**/profiles/sandbox/api/connection-status");
    // Malformed success must not be mistaken for a verified database connection.
    await page.route("**/profiles/sandbox/api/connection-status",route=>route.fulfill({
      status:200,contentType:"application/json",body:JSON.stringify({database:null,role:null})
    }));
    await page.locator("#connection-chooser").selectOption("sandbox");
    await page.locator("#message").getByText(/invalid connection status/).waitFor();
    assert.equal(await page.locator("#connection-chooser").inputValue(),"default");
    assert.equal(await page.locator("#changes-count").textContent(),"1 pending row");
    await page.unroute("**/profiles/sandbox/api/connection-status");
    // A reachable target still requires explicit confirmation before losing state.
    let cancelledPrompt="";
    page.once("dialog",dialog=>{cancelledPrompt=dialog.message();return dialog.dismiss()});
    await page.locator("#connection-chooser").selectOption("sandbox");
    await page.locator("#message").getByText(/Profile switch cancelled; workspace preserved/).waitFor();
    assert.match(cancelledPrompt,/Verified database: datawizz_profile_test/);
    assert.match(cancelledPrompt,/PostgreSQL role: datawizz_test/);
    assert.match(cancelledPrompt,/Access: read-only/);
    assert.equal(await page.locator("#connection-chooser").inputValue(),"default");
    assert.equal(navigation.length,beforeFailureNavigations);
    assert.equal(await page.locator("#changes-count").textContent(),"1 pending row");
    // The authorized, confirmed switch discards both unsaved SQL and staged rows.
    page.once("dialog",dialog=>dialog.accept());
    await page.locator("#connection-chooser").selectOption("sandbox");
    await page.locator("#auth-area").waitFor({state:"visible"});
    await page.locator("#access-token").fill("0123456789abcdefghijklmnopqrstuvwxyz");
    await page.locator("#auth-unlock").click();
    await page.locator("#auth-area").waitFor({state:"hidden"});
    await page.waitForFunction(()=>
      document.getElementById("connection-chooser").value==="sandbox" &&
      document.getElementById("profile-db-status").textContent.includes("datawizz_profile_test"));
    assert.equal(await page.locator("#connection-chooser").inputValue(),"sandbox");
    assert.match(await page.locator("#profile-environment").textContent(),/READ ONLY/);
    assert(!(await page.locator("#sql").inputValue()).includes("must not cross profile boundaries"));
    assert.equal(await page.locator("#changes-bar").isVisible(),false,"Switch must clear staged row drafts");
    assert.equal(await page.locator("#sql-history-list [role=listitem]").count(),0);
    await page.locator("#objects").getByRole("button",{name:"Preview public.profile_only"}).waitFor();
    assert.equal(await page.locator("#objects").getByRole("button",{name:/datawizz_browser_e2e/}).count(),0);
    const erSandbox=await page.request.get(base+"/profiles/sandbox/api/relationships",{headers});
    assert.deepEqual(await erSandbox.json(),[]);
    console.log("Real PostgreSQL browser editing, workspace isolation and ER passed");
  } finally {await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
