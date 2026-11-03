"use strict";
const assert = require("node:assert/strict");
const { chromium } = require("playwright");
const base = "http://127.0.0.1:3188";
const token = "0123456789abcdefghijklmnopqrstuvwxyz";
const headers = {Authorization: "Bearer " + token};

(async () => {
 const browser = await chromium.launch({headless:true});
 try {
  const page = await browser.newPage();
  const anonymous = await page.request.get(base+"/api/managed-connections");
  assert.equal(anonymous.status(),401);
  await page.goto(base+"/?basic-editor=1");
  await page.locator("#auth-area").waitFor({state:"visible"});
  await page.locator("#access-token").fill(token);
  await page.locator("#auth-unlock").click();
  await page.locator("#auth-area").waitFor({state:"hidden"});

  await page.locator("#manage-connections").click();
  await page.locator("#manager-dialog").waitFor({state:"visible"});
  await page.locator("#manager-id").fill("managed-qa");
  await page.locator("#manager-label").fill("Managed QA");
  await page.locator("#manager-host").fill("127.0.0.1");
  await page.locator("#manager-port").fill("5432");
  await page.locator("#manager-database").fill("datawizz_profile_test");
  await page.locator("#manager-username").fill("datawizz_test");
  await page.locator("#manager-password").fill("test_only_password");
  await page.locator("#manager-environment").selectOption("test");
  await page.locator("#manager-test").click();
  await page.waitForFunction(()=>document.getElementById("manager-notice").textContent.includes("Verified: datawizz_profile_test"));
  await page.locator("#manager-save").click();
  await page.waitForFunction(()=>document.getElementById("manager-notice").textContent.includes("Connection saved and verified"));
  assert.equal(await page.locator("#manager-password").inputValue(),"");
  await page.locator("#manager-close").click();

  const list = await page.request.get(base+"/api/managed-connections",{headers});
  assert.equal(list.status(),200);
  const listed = await list.json();
  assert.equal(listed.enabled,true);
  assert.equal(listed.profiles.length,1);
  assert.equal(listed.profiles[0].id,"managed-qa");
  assert(!JSON.stringify(listed).includes("test_only_password"));
  assert(!JSON.stringify(listed).includes("postgres://"));
  assert(!JSON.stringify(listed).includes("ca_file"));

  const scoped=base+"/profiles/managed-qa/api";
  const read=await page.request.post(scoped+"/query",
    {headers,data:{sql:"SELECT label FROM public.profile_only WHERE id=1"}});
  assert.equal(read.status(),200,await read.text());
  assert.equal((await read.json()).rows[0][0],"sandbox-only");
  const denied=await page.request.post(scoped+"/query",
    {headers,data:{sql:"INSERT INTO public.profile_only VALUES (20,'blocked')"}});
  assert.equal(denied.status(),400);
  assert.match((await denied.json()).error,/25006/);
  const visible=await page.request.get(scoped+"/schema",{headers});
  assert.equal(visible.status(),200);
  const objects=await visible.json();
  assert(objects.some(o=>o.name==="profile_only"));
  assert(!objects.some(o=>o.name==="datawizz_browser_e2e"));

  await page.locator("#manage-connections").click();
  await page.locator("#manager-profiles button").first().click();
  assert.equal(await page.locator("#manager-password").inputValue(),"");
  await page.locator("#manager-label").fill("Managed QA renamed");
  await page.locator("#manager-save").click();
  await page.waitForFunction(()=>document.getElementById("manager-notice").textContent.includes("Connection saved and verified"));
  await page.locator("#manager-close").click();
  const renamed=await page.request.get(base+"/api/managed-connections",{headers});
  assert.equal((await renamed.json()).profiles[0].label,"Managed QA renamed");

  const production = {
    id:"unsafe-prod",label:"Unsafe production",environment:"prod",
    read_only:false,host:"127.0.0.1",port:5432,
    database:"datawizz_test",username:"datawizz_test",
    password:"test_only_password",tls:"disable",ca_file:null
  };
  const invalid=await page.request.post(base+"/api/managed-connections",{headers,data:production});
  assert.equal(invalid.status(),400);
  assert(!JSON.stringify(await invalid.json()).includes(production.password));

  page.once("dialog",d=>d.accept());
  await page.locator("#connection-chooser").selectOption("managed-qa");
  // The chooser's confirmation has to be accepted before navigation.
  await page.waitForLoadState("load");
  await page.locator("#auth-area").waitFor({state:"visible"});
  await page.locator("#access-token").fill(token);
  await page.locator("#auth-unlock").click();
  await page.locator("#auth-area").waitFor({state:"hidden"});
  assert.equal(await page.locator("#connection-chooser").inputValue(),"managed-qa");
  assert.match(await page.locator("#profile-db-status").textContent(),/datawizz_profile_test/);
  assert.match(await page.locator("#profile-environment").textContent(),/READ ONLY/);
  console.log("Connection Manager UI, CRUD, authorization and isolated dynamic routing passed");
 } finally {await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
