#!/usr/bin/env node
import fs from "node:fs";
const key=fs.readFileSync(process.env.HOME+"/.local/state/toomux/bonnie-bridge.key","utf8").trim();
const url="http://127.0.0.1:34561/v1/chat/completions";
const jobs=[
  {sid:"failover-a",marker:"FAILOVER_SEED_A"},
  {sid:"failover-b",marker:"FAILOVER_SEED_B"}
];
async function one(j){
  const r=await fetch(url,{method:"POST",headers:{authorization:"Bearer "+key,"content-type":"application/json"},body:JSON.stringify({model:"gpt-5.6-sol",reasoning_effort:"high",provider_session_id:j.sid,stream:false,messages:[{role:"user",content:"Reply with exactly: "+j.marker}]})});
  if(!r.ok)throw new Error(j.sid+" HTTP "+r.status+": "+await r.text());
  const b=await r.json();
  if(b?.choices?.[0]?.message?.content!==j.marker)throw new Error(j.sid+" wrong marker");
}
await Promise.all(jobs.map(one));
const h=await (await fetch("http://127.0.0.1:34561/health")).json();
const owners={};
for(const w of h.workers||[]) for(const s of w.sessions||[]) owners[s]=w.id;
if(!owners["failover-a"]||!owners["failover-b"])throw new Error("missing seeded leases");
if(owners["failover-a"]===owners["failover-b"])throw new Error("expected seed sessions on distinct workers");
console.log(JSON.stringify({owners,workers:h.workers.map(w=>({id:w.id,sessions:w.sessions,healthy:w.healthy}))}));
