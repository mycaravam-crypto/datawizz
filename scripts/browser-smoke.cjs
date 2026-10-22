"use strict";
const assert = require("node:assert/strict");
const http = require("node:http");
const fs = require("node:fs");
const { chromium } = require("playwright");

const html = fs.readFileSync("web/index.html", "utf8");
const server = http.createServer(async (req, res) => {
  res.setHeader("Content-Type", "application/json; charset=utf-8");
  if (req.url === "/") {
    res.setHeader("Content-Type", "text/html; charset=utf-8");
    res.end(html);
  } else if (req.url === "/api/schema") {
    res.end(JSON.stringify([{schema:"public",name:"demo",kind:"table"}]));
  } else if (req.url.startsWith("/api/columns")) {
    res.end(JSON.stringify([{name:"id",data_type:"integer",nullable:false,primary_key:true},{name:"label",data_type:"text",nullable:true,primary_key:false}]));
  } else if (req.url.startsWith("/api/table-rows")) {
    const offset = Number(new URL(req.url,"http://localhost").searchParams.get("offset")||0);
    res.end(JSON.stringify({columns:["id","label"],rows:[[String(offset+1),"original"]],offset,limit:100,has_next:offset===0,stable_order:true}));
  } else if (req.url === "/api/row-change") {\n    let body=""; for await(const chunk of req) body+=chunk;\n    const payload=JSON.parse(body);\n    assert.equal(payload.schema,"public");\n    assert.equal(payload.table,"demo");\n    assert.deepEqual(payload.original,["1","original"]);\n    assert.deepEqual(payload.values,["1","updated"]);\n    res.end(JSON.stringify({affected:1}));\n  } else if (req.url === "/api/query") {
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
    await page.locator("#result-filter").fill("alpha");
    await page.locator("#output").getByText("alpha").waitFor();
    assert.equal(await page.locator("#output tbody tr").count(),1,"Filter shows one loaded row");
    await page.locator("#result-filter").fill("");
    assert.equal(await page.locator("#output tbody tr").count(),3,"Filter reset shows all rows");
    await page.locator("#theme").click();
    assert.equal(await page.locator("html").getAttribute("data-theme"),"light");
    await page.locator("#filter").fill("demo");
    await page.getByRole("button",{name:"Preview public.demo"}).click();
    await page.getByRole("button",{name:"Browse table rows"}).click();
    await page.locator("#server-page-info").getByText(/Database rows 1/).waitFor();
    await page.locator("#output").getByRole("button",{name:"Edit"}).click();
    await page.locator("#row-dialog").waitFor({state:"visible"});
    await page.locator('#row-fields input[data-column="1"]').fill("updated");
    await page.getByRole("button",{name:"Save changes"}).click();
    await page.locator("#row-dialog").waitFor({state:"hidden"});
    await page.locator("#server-next").click();
    await page.locator("#server-page-info").getByText(/Database rows 101/).waitFor();
    await page.getByRole("button",{name:"Run query",exact:false}).click();
    assert.equal(await page.locator("#server-pager").isVisible(),false,"Free SQL hides server pager");
    console.log("Chromium workbench UI smoke tests passed");
  } finally {await browser.close();}
})().catch(e=>{console.error(e);process.exitCode=1}).finally(()=>server.close());
