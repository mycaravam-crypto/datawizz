"use strict";
// Only the default, authenticated API manages server-side profiles.
// Sensitive fields are never written into browser storage or UI metadata.
(() => {
 const byId=id=>document.getElementById(id);
 let profiles=[], editing=null;
 const notice=value=>{byId("manager-notice").textContent=value};
 function fields(){
  return {
   id:byId("manager-id").value.trim(),label:byId("manager-label").value.trim(),
   host:byId("manager-host").value.trim(),port:Number(byId("manager-port").value),
   database:byId("manager-database").value,username:byId("manager-username").value,
   password:byId("manager-password").value,
   environment:byId("manager-environment").value,
   read_only:byId("manager-read-only").checked,tls:byId("manager-tls").value,
   ca_file:byId("manager-ca-file").value.trim()||null
  };
 }
 function constrainProduction(){
  const prod=byId("manager-environment").value==="prod";
  if(prod){byId("manager-read-only").checked=true;byId("manager-tls").value="verify-full"}
  byId("manager-read-only").disabled=prod;
  byId("manager-tls").querySelector('option[value="disable"]').disabled=prod;
 }
 function clearForm(){
  editing=null;byId("manager-form").reset();
  byId("manager-id").readOnly=false;
  byId("manager-port").value="5432";
  byId("manager-read-only").checked=true;
  byId("manager-delete").hidden=true;
  byId("manager-form-heading").textContent="New connection";
  constrainProduction();
  notice("Enter a connection, test it, then save.");
 }
 function edit(profile){
  editing=profile.id;
  for(const field of ["id","label","host","port","database","username","environment","tls"]){
   byId("manager-"+field).value=profile[field];
  }
  byId("manager-read-only").checked=profile.read_only;
  byId("manager-password").value="";
  byId("manager-ca-file").value="";
  byId("manager-id").readOnly=true;
  byId("manager-delete").hidden=false;
  byId("manager-form-heading").textContent="Edit "+profile.label;
  constrainProduction();
  notice("Credentials are hidden. Blank password/CA fields retain existing server-side values.");
 }
 function render(){
  const holder=byId("manager-profiles");holder.replaceChildren();
  if(!profiles.length){
   const node=document.createElement("span");node.className="note";
   node.textContent="No managed connections yet.";holder.append(node);return;
  }
  for(const profile of profiles){
   const button=document.createElement("button");button.type="button";
   button.className="btn small";
   button.textContent=profile.label+" · "+profile.environment.toUpperCase()+
    (profile.read_only?" · read-only":" · writable");
   button.onclick=()=>edit(profile);holder.append(button);
  }
 }
 async function reloadManager(){
  const listing=await api("/api/managed-connections");
  profiles=listing.profiles||[];
  render();
  const disabled=!listing.enabled;
  for(const control of byId("manager-form").querySelectorAll("input,select,button")){
   control.disabled=disabled;
  }
  if(disabled)notice("Management disabled. Configure DATAWIZZ_ACCESS_TOKEN and DATAWIZZ_PROFILE_STORE on the server.");
  else constrainProduction();
 }
 byId("manager-environment").onchange=constrainProduction;
 byId("manage-connections").onclick=async()=>{
  if(busy){info("Finish the current operation before managing connections.");return}
  clearForm();
  byId("manager-dialog").showModal();
  try{await reloadManager()}
  catch(e){
   if(e.status===401){byId("manager-dialog").close();info("Unlock the API before managing connections.");}
   else notice("Unable to load Connection Manager: "+e.message);
  }
 };
 byId("manager-close").onclick=()=>byId("manager-dialog").close();
 byId("manager-dialog").addEventListener("close",()=>{
  byId("manager-password").value="";byId("manager-ca-file").value="";
 });
 byId("manager-new").onclick=clearForm;
 byId("manager-test").onclick=async()=>{
  if(!byId("manager-form").reportValidity())return;
  const button=byId("manager-test");button.disabled=true;
  try{
   const result=await api("/api/managed-connections/test",{
    method:"POST",headers:{"Content-Type":"application/json"},
    body:JSON.stringify(fields())
   });
   notice("Verified: "+result.database+" · "+result.role+
     (result.read_only?" · read-only":" · writable"));
  }catch(e){notice("Connection test failed: "+e.message)}
  finally{button.disabled=false}
 };
 byId("manager-form").onsubmit=async event=>{
  event.preventDefault();
  if(!byId("manager-form").reportValidity())return;
  const data=fields();
  if(!data.read_only&&!confirm("Save a writable database connection? PostgreSQL grants control the actual privileges."))return;
  if(editing===activeProfileId&&!confirm(
      "Changing the active connection reloads DataWizz and discards unsaved workspace state. Continue?"))return;
  const button=byId("manager-save");button.disabled=true;
  try{
   await api(editing?"/api/managed-connections/"+encodeURIComponent(editing):
     "/api/managed-connections",{
    method:editing?"PUT":"POST",headers:{"Content-Type":"application/json"},
    body:JSON.stringify(data)
   });
   byId("manager-password").value="";
   if(editing===activeProfileId){location.reload();return}
   await reloadManager();await loadConnections();clearForm();
   notice("Connection saved and verified.");
  }catch(e){notice("Unable to save: "+e.message)}
  finally{button.disabled=false}
 };
 byId("manager-delete").onclick=async()=>{
  if(!editing)return;
  const id=editing;
  if(!confirm("Permanently delete managed connection '"+id+"' from the server?"))return;
  if(id===activeProfileId&&!confirm(
      "This active connection will close and unsaved workspace data will be discarded. Continue?"))return;
  const button=byId("manager-delete");button.disabled=true;
  try{
   await api("/api/managed-connections/"+encodeURIComponent(id),{method:"DELETE"});
   byId("manager-password").value="";
   if(id===activeProfileId){
    try{sessionStorage.removeItem(profileStorageKey)}catch{}
    location.reload();return;
   }
   await reloadManager();await loadConnections();clearForm();
   notice("Managed connection deleted.");
  }catch(e){notice("Unable to delete: "+e.message)}
  finally{button.disabled=false}
 };
})();
