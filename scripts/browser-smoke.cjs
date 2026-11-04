"use strict";
const assert = require("node:assert/strict");
const http = require("node:http");
const fs = require("node:fs");
const { chromium } = require("playwright");

const html = fs.readFileSync("web/index.html", "utf8");
let rejectNextChangeset = false;
let largeErFixture = false;
let emptyErFixture = false;
let delayNextBrowse = false;
let releaseBrowse = null;
const server = http.createServer(async (req, res) => {
  res.setHeader("Content-Type", "application/json; charset=utf-8");
  if (req.url === "/" || req.url.startsWith("/?")) {
    res.setHeader("Content-Type", "text/html; charset=utf-8");
    res.end(html);
  } else if (req.url === "/assets/sql-editor.js") {
    res.setHeader("Content-Type","application/javascript; charset=utf-8");
    res.end(fs.readFileSync("web/vendor/sql-editor.js","utf8"));
  } else if (req.url === "/api/admin") {
    res.end(JSON.stringify({
      databases:[{name:"test",owner:"test",allow_connections:true}],
      roles:[{name:"<img src=x onerror=alert(1)>",can_login:false,superuser:false,create_db:false,create_role:false}],
      memberships:[],sessions:[],extensions:[],schemas:[],
      relations:[{schema:"public",name:"demo_view",kind:"view",owner:"test",estimated_rows:0,size_bytes:0,definition:"SELECT id FROM public.demo"},
        {schema:"public",name:"demo",kind:"table",owner:"test",estimated_rows:0,size_bytes:0},
        {schema:"public",name:"other",kind:"table",owner:"test",estimated_rows:0,size_bytes:0}],
      indexes:[{schema:"public",table:"demo",name:"demo_idx",valid:true,scans:0,definition:"CREATE INDEX demo_idx ON public.demo USING btree (id)"}],
      constraints:[],table_stats:[],locks:[],settings:[],privileges:[],routines:[]
    }));
  } else if (req.url === "/api/connections") {
    res.end(JSON.stringify([{id:"default",label:"Default PostgreSQL",engine:"postgresql",environment:"dev",read_only:false}]));
  } else if (req.url === "/api/connection-status") {
    res.end(JSON.stringify({database:"test",role:"test",read_only:false}));
  } else if (req.url === "/api/test-er-large") {
    largeErFixture=true;emptyErFixture=false;
    res.end(JSON.stringify({enabled:true}));
  } else if (req.url === "/api/test-er-reset") {
    largeErFixture=false;emptyErFixture=false;
    res.end(JSON.stringify({enabled:false}));
  } else if (req.url === "/api/test-er-empty") {
    largeErFixture=false;emptyErFixture=true;
    res.end(JSON.stringify({enabled:true}));
  } else if (req.url === "/api/relationships") {
    const edges=emptyErFixture?[]:largeErFixture?Array.from({length:80},(_,i)=>({
      from_schema:"public",from_table:"table_"+i%40,
      to_schema:"public",to_table:"table_"+(i+1)%40,
      constraint:"fk_"+i,definition:"FOREIGN KEY (id) REFERENCES public.table_"+(i+1)%40+"(id)"
    })): [{from_schema:"public",from_table:"demo",to_schema:"public",
      to_table:"other",constraint:'fk_<img src=x onerror=alert(1)>',
      definition:"FOREIGN KEY (other_id) REFERENCES other(id)"}];
    res.end(JSON.stringify(edges));
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
    const params=new URL(req.url,"http://localhost").searchParams;
    const offset=Number(params.get("offset")||0),limit=Number(params.get("limit")||100);
    if(params.has("filter")||params.has("sort_column")){
      assert.equal(params.get("table"),"demo","Filter and export must target the selected relation");
      const filter=params.get("filter")||"";
      if(filter==="many"){
        const rows=Array.from({length:limit},(_,i)=>[String(offset+i+1),"many-"+(offset+i+1)]);
        res.end(JSON.stringify({columns:["id","label"],rows,offset,limit,has_next:true,stable_order:true}));
      }else{
        let data=[["1","Alpha_One"],["2","Beta%Two"],["3","alpha_Three"],["4","=1+1"]];
        if(params.has("filter"))data=data.filter(row=>row[1].toLowerCase().includes(filter.toLowerCase()));
        if(params.has("filter2")){
          const column=params.get("filter2_column"),value=params.get("filter2").toLowerCase();
          assert(["id","label"].includes(column));
          data=data.filter(row=>row[column==="id"?0:1].toLowerCase().includes(value));
        }
        if(params.get("sort_column")==="label")
          data.sort((a,b)=>params.get("sort_dir")==="desc"?b[1].localeCompare(a[1]):a[1].localeCompare(b[1]));
        res.end(JSON.stringify({columns:["id","label"],rows:data.slice(offset,offset+limit),offset,limit,has_next:offset+limit<data.length,stable_order:true}));
      }
    }else{
      res.end(JSON.stringify({columns:["id","label"],rows:[[String(offset+1),"original"]],offset,limit:100,has_next:offset===0,stable_order:true}));
    }
  } else if (req.url.startsWith("/api/table-export?")) {
    const params=new URL(req.url,"http://localhost").searchParams;
    assert.equal(params.get("table"),"demo");
    assert.equal(params.has("offset"),false,"Export must use a single database snapshot");
    assert.equal(params.has("limit"),false,"The server enforces export quotas");
    const filter=params.get("filter")||"";
    if(filter==="many"){
      res.statusCode=413;
      res.end(JSON.stringify({error:"CSV export exceeds 10,000 rows; narrow the filter"}));
    }else{
      if(filter==="slow"){
        await new Promise(resolve=>{
          const timeout=setTimeout(resolve,2000);
          res.once("close",()=>{clearTimeout(timeout);resolve()});
        });
      }
      if(!res.destroyed){
        const cells=[["1","Alpha_One"],["2","Beta%Two"],["3","alpha_Three"],["4","=1+1"]];
        let rows=filter?cells.filter(row=>row[1].toLowerCase().includes(filter.toLowerCase())):cells;
        if(params.has("filter2")){
          const column=params.get("filter2_column"),value=params.get("filter2").toLowerCase();
          assert(["id","label"].includes(column));
          rows=rows.filter(row=>row[column==="id"?0:1].toLowerCase().includes(value));
        }
        const escape=value=>'"'+String(value).replace(/^[\s\uFEFF\u200B]*[=+@-]/u,match=>"'"+match).replaceAll('"','""')+'"';
        const csv="\uFEFF"+[["id","label"],...rows].map(row=>row.map(escape).join(",")).join("\r\n");
        res.setHeader("Content-Type","text/csv; charset=utf-8");
        res.setHeader("Cache-Control","no-store");
        res.end(csv);
      }
    }
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
    const openRowMore=async()=>{if(!await page.locator("#row-more").evaluate(node=>node.open))await page.locator("#row-more summary").click()};
    await page.goto(origin+"?basic-editor=1");
    await page.getByRole("heading",{name:"SQL Workbench"}).waitFor();
    assert.equal(await page.locator("#csv").evaluate(el=>getComputedStyle(el).opacity),"1",
      "Disabled actions remain legible instead of fading to low contrast");
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
    await page.locator("#app-menu summary").click();
    await page.locator("#theme").click();
    assert.equal(await page.locator("html").getAttribute("data-theme"),"light");
    // Top-level navigation has a single compact tab strip; connection context remains visible.
    assert.equal(await page.locator(".topbar #workspace-tab-sql").count(),1);
    assert.equal(await page.locator(".topbar #workspace-tab-admin").count(),1);
    assert.equal(await page.locator("#profile-environment").isVisible(),true);
    assert.equal(await page.locator("#manage-connections").isVisible(),false,
      "Connection management belongs to Settings");
    // Navigation remains usable without horizontal overflow at tablet and phone widths.
    for(const width of [768,390]){
      await page.setViewportSize({width,height:844});
      const header=page.locator(".topbar");
      const sizing=await header.evaluate(el=>{
        const tab=el.querySelector(".workspace-switcher"),badge=el.querySelector("#profile-environment");
        const bounds=el.getBoundingClientRect(),nav=tab.getBoundingClientRect();
        return {overflow:el.scrollWidth>el.clientWidth+1,within:nav.right<=bounds.right+1&&nav.left>=bounds.left-1,
          badgeVisible:badge.getBoundingClientRect().width>0};
      });
      assert.equal(sizing.overflow,false,"Top navigation must not overflow at "+width+"px");
      assert.equal(sizing.within,true,"Workspace tabs must stay inside header at "+width+"px");
      assert.equal(sizing.badgeVisible,true,"Environment access status remains visible at "+width+"px");
    }
    await page.setViewportSize({width:1280,height:800});

    await page.locator("#filter").fill("demo");
    const unfinishedSQL=await page.locator("#sql").inputValue();
    const draftTab=await page.locator("#tabs .tab.active").textContent();
    await page.getByRole("button",{name:"Preview public.demo"}).click();
    await page.waitForFunction(() => tableMode && !busy);
    assert.equal(await page.locator("#sql").inputValue(),unfinishedSQL,
      "Selecting a table must not overwrite the active SQL draft");
    assert.equal(await page.locator("#editor-body").isVisible(),false,
      "Selecting a table focuses the results workspace, not the SQL editor");
    assert.equal(await page.locator("#sql").evaluate(el=>document.activeElement===el),false,
      "Selecting a table does not unexpectedly move keyboard focus to SQL");
    assert.equal(await page.locator("#objects .object.selected").evaluate(el=>document.activeElement===el),true,
      "Selection rerender must retain keyboard focus on the selected explorer object");
    assert.equal(await page.locator("#row-edit").evaluate(el=>el.classList.contains("primary")),false,
      "A disabled Edit row action is not presented as the primary action");
    assert.match(await page.locator("#row-action-hint").textContent(),/Select exactly one row/);
    await page.locator("#object-open-sql").click();
    assert.equal(await page.locator("#editor-body").isVisible(),true,
      "Open SELECT explicitly reveals the editor");
    assert.match(await page.locator("#sql").inputValue(),/^SELECT \* FROM "public"\."demo" LIMIT 200;/);
    await page.getByRole("tab",{name:draftTab,exact:true}).click();
    assert.equal(await page.locator("#sql").inputValue(),unfinishedSQL,
      "The existing SQL draft survives opening a generated SELECT in a separate tab");
    await page.locator("#editor-toggle").click();
    assert.equal(await page.locator("#editor-body").isVisible(),false);
    assert.equal(await page.locator("#output .grid tbody tr").count()>0,true,
      "Selecting a table automatically loads rows without a manual refresh");
    assert.equal(await page.locator("#edit-mode").isDisabled(),false,
      "Inline edit option is available after loading a table with a primary key");
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
    assert.equal(await page.locator("#row-action-hint").textContent().then(s=>s.includes("Select exactly one row")),true,
      "Explains how to edit without relying on disabled-button tooltips");
    assert.equal(await page.locator("#row-edit").isDisabled(),true,"Edit row needs one selected row");
    assert.equal(await page.locator("#output input[aria-label^='Select row']").count()>0,true,
      "Rows are selectable before inline edit mode is enabled");
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
    assert.equal(await page.locator("#changes-bar #changes-undo").isVisible(),true,
      "Undo is visible beside staged changes, not hidden in More actions");
    assert.equal(await page.locator("#changes-undo").isDisabled(),false);
    // A focused editable cell must support directional keyboard navigation.
    await page.locator("#output .editable-cell").first().focus();
    await page.locator("#output .editable-cell").first().press("ArrowDown");
    assert.equal(await page.locator("#output .editable-cell:focus").count(),1);

    await page.locator("#output input[aria-label^='Select row']").first().check();
    assert.equal(await page.locator("#row-actions .table-edit-actions > button").count(),2,
      "Only Edit row and Edit cells remain as direct row-edit actions");
    assert.equal(await page.locator("#row-delete").isVisible(),false);
    assert.equal(await page.locator("#row-edit").isDisabled(),true,
      "Direct edit is blocked while staged changes exist");
    assert.match(await page.locator("#row-action-hint").textContent(),/Review & save or discard/);
    await openRowMore();
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
    assert.equal(await page.locator("#edit-mode").getAttribute("aria-pressed"),"true",
      "Refreshing rows must retain edit mode");
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
    await page.waitForFunction(() => !busy);
    assert.equal(await page.locator("#browse").isEnabled(),true,
      "Refresh rows is re-enabled after legacy server paging completes");
    await page.evaluate(()=>fetch("/api/test-conflict"));
    await page.locator("#browse").click();
    await page.waitForFunction(() => !busy);
    assert.equal(await page.locator("#edit-mode").getAttribute("aria-pressed"),"true",
      "Refresh after paging must retain inline edit mode");
    await page.locator("#output .editable-cell").first().dblclick();
    await page.locator("#output .cell-editor").fill("retry-value");
    await page.locator("#output .cell-editor").press("Enter");
    await page.locator("#changes-save").click();
    await page.locator("#changes-review-confirm").click();
    await page.getByText(/No staged rows committed/).waitFor();
    await page.locator("#changes-count").getByText("1 pending row").waitFor();
    assert.equal(await page.locator("#output .pending-cell").count(),1,"Conflict retains draft");
    page.once("dialog",dialog=>dialog.accept());
    // The editor is collapsed in table-browse mode; explicitly reopen it for SQL execution.
    await page.locator("#editor-toggle").click();
    await page.getByRole("button",{name:"Run query",exact:false}).click();
    await page.locator("#server-pager").waitFor({state:"hidden"});
    assert.equal(await page.locator("#server-pager").isVisible(),false,"Free SQL hides server pager");
    await page.waitForFunction(() => !busy);
    assert.equal(await page.locator("#browse").isEnabled(),true,
      "Loading table rows is available again after a successful SQL query");
    assert.match(await page.locator("#row-action-hint").textContent(),/SQL results are read-only/,
      "The interface explains why SQL result rows cannot be edited");
    await page.route("**/api/query",route=>route.fulfill({
      status:500,contentType:"application/json",body:JSON.stringify({error:"Simulated SQL failure"})
    }));
    await page.getByRole("button",{name:/Run query/}).click();
    await page.getByText("Query failed.").waitFor();
    await page.waitForFunction(() => !busy);
    assert.equal(await page.locator("#browse").isEnabled(),true,
      "Loading table rows remains available after a failed SQL query");
    await page.unroute("**/api/query");
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
    assert.equal(await page.locator("#admin-nav .admin-tree-button").count(),17);
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
    // ER navigation is read-only, keyboard-operable, zoomable and backed by a text table.
    await page.locator("#admin-er-nav").click();
    await page.locator("#er-accessible tbody tr").first().waitFor();
    assert.equal(await page.locator("#er-diagram").getAttribute("role"),"region");
    assert.equal(await page.locator("#er-diagram").getAttribute("tabindex"),"0");
    assert.equal(await page.locator("#er-accessible caption").count(),1);
    assert.equal(await page.locator("#er-accessible img").count(),0,"FK names cannot inject HTML");
    const nativeWidth=Number(await page.locator("#er-diagram svg").getAttribute("width"));
    await page.locator("#er-zoom-in").click();
    assert(Number(await page.locator("#er-diagram svg").getAttribute("width"))>nativeWidth);
    await page.locator("#er-zoom-reset").click();
    assert.equal(Number(await page.locator("#er-diagram svg").getAttribute("width")),nativeWidth);
    await page.locator("#er-diagram").focus();
    await page.keyboard.press("+");
    assert.equal(await page.locator("#er-scale").textContent(),"125%");
    await page.keyboard.press("0");
    assert.equal(await page.locator("#er-scale").textContent(),"100%");
    const erNode=page.locator('#er-diagram .er-node[aria-label="Inspect relation public.demo"]');
    await erNode.focus();await erNode.press("Enter");
    assert.equal(await page.locator("#admin-view-title").textContent(),"Relations");
    assert.match(await page.locator("#admin-detail").textContent(),/demo/);
    await page.locator("#admin-er-nav").click();
    await page.locator('#er-accessible button[aria-label="Inspect target relation public.other"]').click();
    assert.equal(await page.locator("#admin-view-title").textContent(),"Relations");
    assert.match(await page.locator("#admin-detail").textContent(),/other/);
    await page.evaluate(()=>fetch("/api/test-er-large"));
    await page.locator("#admin-er-nav").click();
    await page.locator("#er-refresh").click();
    await page.waitForFunction(()=>document.querySelectorAll("#er-diagram .er-node").length===40);
    assert.equal(await page.locator("#er-accessible tbody tr").count(),80);
    await page.locator("#er-fit").click();
    await page.locator("#er-diagram").focus();
    await page.keyboard.press("ArrowDown");
    const scrollY=await page.locator("#er-diagram").evaluate(panel=>panel.scrollTop);
    assert(scrollY>0,"Keyboard arrows must pan the viewport on large ER diagrams");
    await page.evaluate(()=>fetch("/api/test-er-reset"));
    await page.locator("#er-refresh").click();
    await page.waitForFunction(()=>document.getElementById("er-status").textContent.startsWith("1 relationships"));
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
    // Query history is bounded and memory-only; reopening never executes SQL.
    await page.locator("#sql-productivity").evaluate(el=>el.open=true);
    assert((await page.locator("#sql-history-count").textContent()).includes("History: "));
    await page.locator("#sql-history-search").fill("EXPLAIN");
    assert((await page.locator("#sql-history-list [role=listitem]").count())>=1);
    let productivityPosts=0;
    await page.route("**/api/query",route=>{productivityPosts++;return route.continue()});
    await page.locator("#sql-history-list [role=listitem]").first().getByRole("button",{name:"Open"}).click();
    assert((await page.locator("#sql").inputValue()).includes("EXPLAIN (FORMAT JSON)"));
    assert.equal(productivityPosts,0,"Opening history must not execute SQL");

    // Explicit Ctrl+Space suggestions reuse available metadata; no extra API calls.
    await page.locator("#sql").fill("SEL");
    await page.locator("#sql").press("Control+Space");
    await page.locator("#sql-completion button").first().waitFor();
    assert((await page.locator("#sql-completion button").first().textContent()).includes("SELECT"));
    await page.locator("#sql-completion button").first().click();
    assert.equal(await page.locator("#sql").inputValue(),"SELECT");
    await page.locator("#sql").fill("dem");
    await page.locator("#sql").press("Control+Space");
    await page.locator("#sql-completion button").first().waitFor();
    assert((await page.locator("#sql-completion button").first().textContent()).includes("public.demo"));
    await page.locator("#sql-completion button").first().click();
    assert.equal(await page.locator("#sql").inputValue(),'"public"."demo"');
    assert.equal(productivityPosts,0,"Completion must not execute SQL");

    // Saving is opt-in, persists only named scripts, and never persists session history.
    await page.locator("#sql").fill("SELECT 'saved query'");
    await page.locator("#sql-script-name").fill("Saved <img src=x onerror=alert(1)>");
    page.once("dialog",dialog=>dialog.accept());
    await page.locator("#sql-script-save").click();
    assert.equal(await page.locator("#sql-scripts-list [role=listitem]").count(),1);
    assert.equal(await page.locator("#sql-scripts-list img").count(),0,"Script names must render as text");
    assert.equal(productivityPosts,0);
    await page.reload();
    await page.locator("#sql-productivity").evaluate(el=>el.open=true);
    assert.equal(await page.locator("#sql-history-list [role=listitem]").count(),0,"History is not persisted");
    assert.equal(await page.locator("#sql-scripts-list [role=listitem]").count(),1,"Saved script survives reload");
    await page.locator("#sql-scripts-list [role=listitem]").getByRole("button",{name:"Open"}).click();
    assert.equal(await page.locator("#sql").inputValue(),"SELECT 'saved query'");
    assert.equal(productivityPosts,0);
    page.once("dialog",dialog=>dialog.accept());
    await page.locator("#sql-scripts-list").getByRole("button",{name:/Delete saved script/}).click();
    assert.equal(await page.locator("#sql-scripts-list [role=listitem]").count(),0);
    const stored=await page.evaluate(()=>JSON.parse(localStorage.getItem("datawizz.saved-sql.v1")));
    assert.deepEqual(stored.scripts,[]);
    await page.unroute("**/api/query");
    // Data Explorer 2.0: database-side filter/sort, bounded full export, no SQL write.
    await page.locator("#filter").fill("");
    await page.getByRole("button",{name:"Preview public.demo"}).click();
    await page.locator("#table-options summary").click();
    await page.locator("#table-filter-column").waitFor({state:"visible"});
    await page.waitForFunction(() => document.querySelectorAll("#table-filter-column option").length>1);
    await page.locator("#table-filter-column").selectOption("label");
    await page.locator("#table-filter-value").fill("alpha_");
    await page.locator("#table-sort-column").selectOption("label");
    await page.locator("#table-sort-dir").selectOption("desc");
    const filtered=page.waitForResponse(r=>r.url().includes("/api/table-rows?")&&r.url().includes("filter=alpha_"));
    await page.locator("#table-apply").click();
    const filterResponse=await filtered;
    const query=new URL(filterResponse.url()).searchParams;
    assert.equal(query.get("filter_column"),"label");
    assert.equal(query.get("sort_column"),"label");
    assert.equal(query.get("sort_dir"),"desc");
    await page.locator("#output").getByText("alpha_Three",{exact:true}).waitFor();
    assert.equal(await page.locator("#output").getByText("Alpha_One",{exact:true}).count(),1);
    assert.equal(await page.locator("#output").getByText("Beta%Two",{exact:true}).count(),0);
    assert.equal(await page.locator("#output .grid tbody tr").count(),2);
    // Two database-side conditions are AND-connected in paging and CSV export.
    await page.locator("#table-filter2-value").fill("3");
    await page.locator("#table-apply").click();
    await page.locator("#message").getByText(/Choose the second filter column/).waitFor();
    await page.locator("#table-filter2-column").selectOption("id");
    const andResponse=page.waitForResponse(r=>r.url().includes("/api/table-rows?")&&
      r.url().includes("filter2=3"));
    await page.locator("#table-apply").click();
    const combined=await andResponse;
    assert.equal(new URL(combined.url()).searchParams.get("filter2_column"),"id");
    await page.locator("#output").getByText("alpha_Three",{exact:true}).waitFor();
    assert.equal(await page.locator("#output .grid tbody tr").count(),1);
    const filteredDownload=page.waitForEvent("download");
    await page.locator("#table-export").click();
    const filteredCsv=fs.readFileSync(await (await filteredDownload).path(),"utf8");
    assert(filteredCsv.includes("alpha_Three"));
    assert(!filteredCsv.includes("Alpha_One"),"Snapshot export must apply second filter");
    await page.locator("#table-filter2-value").fill("");
    await page.locator("#table-filter2-column").selectOption("");
    // Filter a formula-like value; downloaded CSV must be spreadsheet-safe.
    await page.locator("#table-filter-value").fill("=1+1");
    await page.locator("#table-apply").click();
    await page.locator("#output").getByText("=1+1",{exact:true}).waitFor();
    const downloading=page.waitForEvent("download");
    await page.locator("#table-export").click();
    const download=await downloading;
    const csvBody=fs.readFileSync(await download.path(),"utf8");
    assert(csvBody.includes('"\'=1+1"'),"CSV must neutralize spreadsheet formulas");
    assert(csvBody.includes('"id","label"'));
    // Quota violations must return 413 without a partial CSV file.
    await page.locator("#table-filter-value").fill("many");
    await page.locator("#table-apply").click();
    await page.locator("#output").getByText("many-1",{exact:true}).waitFor();
    await page.locator("#table-export").click();
    await page.locator("#message").getByText(/exceeds 10,000 rows/).waitFor();
    // Abort is user-controlled; no stale CSV download is reported as success.
    await page.locator("#table-filter-value").fill("slow");
    await page.locator("#table-apply").click();
    await page.waitForFunction(() => !busy);
    await page.locator("#table-export").click();
    await page.locator("#table-export-cancel").click();
    await page.locator("#message").getByText(/Export cancelled; no file downloaded/).waitFor();
    await page.locator("#table-reset").click();
    await page.locator("#output").getByText("original",{exact:true}).waitFor();
    // Read-only ER overview remains text-only for untrusted PostgreSQL names.
    await page.locator("#workspace-tab-admin").click();
    await page.locator("#admin-er-nav").click();
    await page.locator("#er-accessible tbody tr").first().waitFor();
    assert.equal(await page.locator("#er-diagram svg").count(),1);
    assert.match(await page.locator("#er-status").textContent(),/1 relationships/);
    assert.equal(await page.locator("#er-accessible img").count(),0);
    assert.equal(await page.locator("#er-diagram img").count(),0);
    assert.match(await page.locator("#er-accessible").textContent(),/fk_<img src=x onerror=alert\(1\)>/);
    await page.evaluate(()=>fetch("/api/test-er-empty"));
    await page.locator("#er-refresh").click();
    await page.waitForFunction(()=>document.getElementById("er-status").textContent==="No foreign keys");
    assert.equal(await page.locator("#er-diagram svg").count(),0);
    assert.equal(await page.locator("#er-zoom-in").isDisabled(),true,"No zoom on an empty graph");
    assert.match(await page.locator("#er-accessible").textContent(),/No visible foreign keys/);
    assert.equal(await page.locator("#connection-chooser").isDisabled(),true,
      "Single-profile default preserves the no-switch behavior");
    // Rich editor is the default. PostgreSQL highlighting and key bindings remain offline.
    const rich=await browser.newPage({viewport:{width:1280,height:800},acceptDownloads:true});
    const richErrors=[];
    rich.on("console",message=>{if(["warning","error"].includes(message.type()))richErrors.push(message.text())});
    rich.on("pageerror",error=>richErrors.push(error.message));
    try{
      await rich.goto(origin);
      const mode=await rich.evaluate(()=>({
        editor:typeof window.DataWizzEditor,
        hidden:document.getElementById("sql-rich").hidden,
        html:document.getElementById("sql-rich").children.length,
        basic:new URLSearchParams(location.search).has("basic-editor")
      }));
      assert.equal(mode.editor,"object","Editor bundle not available: "+JSON.stringify({mode,richErrors}));
      assert.equal(mode.html>0,true,"Editor failed to initialize: "+JSON.stringify({mode,richErrors}));
      await rich.locator("#sql-rich .cm-editor").waitFor({state:"visible"});
      assert.equal(await rich.locator("#sql").isVisible(),false);
      assert((await rich.locator("#sql-rich .cm-gutters").count())>=1,
        "Rich editor must show line numbers");
      const edit=rich.locator("#sql-rich .cm-content");
      await edit.click();
      await rich.keyboard.press("ControlOrMeta+A");
      await rich.keyboard.insertText("SELECT 7");
      assert.equal(await rich.evaluate(()=>document.getElementById("sql").value),"SELECT 7");
      await rich.keyboard.press("ControlOrMeta+Enter");
      await rich.waitForFunction(() => document.getElementById("sql-history-count").textContent.includes("History: 1"));
      assert.equal((await rich.evaluate(()=>document.getElementById("sql").value)),"SELECT 7");
      await rich.locator("#sql-productivity").evaluate(el=>el.open=true);
      await rich.locator("#sql").evaluate(el=>el.value="SEL");
      assert.equal(await rich.evaluate(()=>document.getElementById("sql").value),"SEL");
      await edit.click();
      await rich.keyboard.press("ControlOrMeta+Space");
      await rich.locator(".cm-tooltip-autocomplete").waitFor({state:"visible"});
      // Draft storage is off until the user explicitly opts in.
      assert.equal(await rich.evaluate(()=>localStorage.getItem("datawizz.sql-draft.v1.default")),null);
      rich.once("dialog",dialog=>dialog.accept());
      await rich.locator("#sql-draft-consent").check();
      await rich.waitForFunction(()=>Boolean(localStorage.getItem("datawizz.sql-draft.v1.default")));
      await rich.locator("#sql").evaluate(el=>el.value="SELECT 'recovery'");
      await rich.waitForFunction(()=>{
        const x=JSON.parse(localStorage.getItem("datawizz.sql-draft.v1.default")||"null");
        return x?.tabs?.some(t=>t.sql==="SELECT 'recovery'");
      });
      await rich.reload();
      await rich.locator("#sql-rich .cm-editor").waitFor({state:"visible"});
      assert.notEqual(await rich.evaluate(()=>document.getElementById("sql").value),"SELECT 'recovery'",
        "Restore must never happen without a user action");
      rich.once("dialog",dialog=>dialog.accept());
      await rich.locator("#sql-productivity").evaluate(el=>el.open=true);
      await rich.locator("#sql-draft-restore").click();
      assert.equal(await rich.evaluate(()=>document.getElementById("sql").value),"SELECT 'recovery'");
      rich.once("dialog",dialog=>dialog.accept());
      await rich.locator("#sql-draft-clear").click();
      assert.equal(await rich.evaluate(()=>localStorage.getItem("datawizz.sql-draft.v1.default")),null);
      assert.equal(await rich.evaluate(()=>localStorage.getItem("datawizz.sql-draft-opt-in.v1.default")),null);
    }finally{await rich.close()}
    console.log("Chromium workbench UI smoke tests passed");
  } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>server.close());
