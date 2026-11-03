"use strict";
const assert = require("node:assert/strict");

const base = "http://127.0.0.1:3188/profiles/reader/api";
const authorization = "Bearer " + process.env.DATAWIZZ_ACCESS_TOKEN;
const headers = { Authorization: authorization };

async function request(path, method = "GET", payload) {
  const response = await fetch(base + path, {
    method, headers: { ...headers, ...(payload ? { "Content-Type": "application/json" } : {}) },
    body: payload ? JSON.stringify(payload) : undefined,
  });
  const text = await response.text();
  return { status: response.status, text };
}
(async () => {
  const status = await request("/connection-status");
  assert.equal(status.status, 200, status.text);
  assert.equal(JSON.parse(status.text).role, "datawizz_reader");

  const schema = await request("/schema");
  assert.equal(schema.status, 200, schema.text);
  const visible = JSON.parse(schema.text);
  assert(visible.some(object => object.name === "datawizz_browser_e2e"));
  assert(!visible.some(object => object.name === "datawizz_private_e2e"));

  const allowed = await request("/query", "POST", {
    sql: "SELECT label FROM public.datawizz_browser_e2e WHERE id=1",
  });
  assert.equal(allowed.status, 200, allowed.text);

  const forbidden = await request("/query", "POST", {
    sql: "UPDATE public.datawizz_browser_e2e SET label='blocked' WHERE id=1",
  });
  assert.equal(forbidden.status, 400, forbidden.text);
  assert.match(forbidden.text, /42501/);

  const inserted = await request("/row-insert", "POST", {
    schema: "public", table: "datawizz_browser_e2e",
    fields: [{ column: "id", value: "999" }, { column: "label", value: "blocked" }],
  });
  assert.equal(inserted.status, 403, inserted.text);

  const hidden = await request("/table-rows?schema=public&table=datawizz_private_e2e&offset=0&limit=10");
  assert.notEqual(hidden.status, 200);
  const unknown = await request("/table-rows?schema=public&table=profile_only&offset=0&limit=10");
  assert.notEqual(unknown.status, 200);

  for (const result of [status, schema, allowed, forbidden, inserted, hidden, unknown]) {
    assert(!result.text.includes("postgres://"));
    assert(!result.text.includes("reader_test_password"));
  }
  console.log("Restricted PostgreSQL LOGIN role verified over authenticated HTTP");
})().catch(error => { console.error(error); process.exitCode = 1; });
