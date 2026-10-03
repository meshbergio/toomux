#!/usr/bin/env node
import fs from "node:fs";
const key=fs.readFileSync(process.env.HOME+"/.local/state/toomux/bonnie-bridge.key","utf8").trim();
const api="http://127.0.0.1:34561/v1/chat/completions";
const sid="effort-affinity";
async function call(effort,marker){
  const r=await fetch(api,{method:"POST",headers:{authorization:"Bearer "+key,"content-type":"application/json"},body:JSON.stringify({
    model:"gpt-5.6-sol",reasoning_effort:effort,provider_session_id:sid,stream:false,
    messages:[{role:"user",content:"Reply with exactly: "+marker}]
  })});
  if(!r.ok) throw new Error(effort+" HTTP "+r.status+": "+await r.text());
  const b=await r.json();
  if(b?.choices?.[0]?.message?.content!==marker) throw new Error(effort+" wrong marker");
  if(b?.reasoning_effort!==effort) throw new Error(effort+" response attested "+b?.reasoning_effort);
  if(b?.reasoning_effort!==effort) throw new Error(effort+" response attested "+b?.reasoning_effort);
}
async function owner(){
  const h=await (await fetch("http://127.0.0.1:34561/readyz")).json();
  const w=(h.workers||[]).find(x=>(x.sessions||[]).includes(sid));
  if(!w) throw new Error("missing sticky lease");
  return {id:w.id,effort:w.effort};
}
await call("medium","AFFINITY_MEDIUM_OK");
const a=await owner();
await call("high","AFFINITY_HIGH_OK");
const b=await owner();
console.log(JSON.stringify({medium:a,high:b}));
if(a.id!==b.id) throw new Error("sticky session moved workers");
if(b.effort!=="high") throw new Error("worker did not return to high");
console.log("EFFORT_AFFINITY_ACCEPTANCE=PASS");
