"use strict";
const assert = require("node:assert/strict");
const base="http://127.0.0.1:3188";
const headers={Authorization:"Bearer 0123456789abcdefghijklmnopqrstuvwxyz"};

(async()=>{
 const list=await fetch(base+"/api/managed-connections",{headers});
 assert.equal(list.status,200);
 const profiles=await list.json();
 assert.equal(profiles.enabled,true);
 assert(profiles.profiles.some(p=>p.id==="managed-qa"&&p.label==="Managed QA renamed"),
   "Managed profiles must survive application restart");
 assert(!JSON.stringify(profiles).includes("test_only_password"));
 const active=await fetch(base+"/profiles/managed-qa/api/connection-status",{headers});
 assert.equal(active.status,200);
 const status=await active.json();
 const {statSync}=require('node:fs');
 const store=statSync(process.env.DATAWIZZ_PROFILE_STORE);
 assert.equal(store.mode & 0o777,0o600,'Profile file must have 0600 permissions');
 assert.equal(status.database,"datawizz_profile_test");
 assert.equal(status.read_only,true);

 const removed=await fetch(base+"/api/managed-connections/managed-qa",
   {method:"DELETE",headers});
 assert.equal(removed.status,204);
 const after=await fetch(base+"/api/connections",{headers});
 assert.equal(after.status,200);
 assert(!(await after.json()).some(p=>p.id==="managed-qa"));
 const missing=await fetch(base+"/profiles/managed-qa/api/schema",{headers});
 assert.equal(missing.status,404);
 const invalid=await fetch(base+"/api/managed-connections/unknown",
   {method:"DELETE",headers});
 assert.equal(invalid.status,404);
 console.log("Managed PostgreSQL profiles persist across restarts and delete safely");
})().catch(e=>{console.error(e);process.exitCode=1});
