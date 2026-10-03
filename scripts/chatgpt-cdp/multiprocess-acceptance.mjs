#!/usr/bin/env node
import fs from "node:fs";
const key=fs.readFileSync(process.env.HOME+"/.local/state/toomux/bonnie-bridge.key","utf8").trim();
const jobs=[
  {port:34581,sid:"proc-a",marker:"PROCESS_A_OK"},
  {port:34582,sid:"proc-b",marker:"PROCESS_B_OK"},
];
const start=performance.now();
async function one(j){
  const t0=performance.now();
  const r=await fetch(`http://127.0.0.1:${j.port}/v1/chat/completions`,{
    method:"POST",
    headers:{authorization:"Bearer "+key,"content-type":"application/json"},
    body:JSON.stringify({model:"gpt-5.6-sol",reasoning_effort:"high",provider_session_id:j.sid,stream:false,messages:[{role:"user",content:"Reply with exactly: "+j.marker}]})
  });
  if(!r.ok) throw new Error(j.sid+" HTTP "+r.status+": "+await r.text());
  const b=await r.json();
  if(b?.choices?.[0]?.message?.content!==j.marker) throw new Error(j.sid+" wrong marker");
  return {sid:j.sid,start_ms:Math.round(t0-start),end_ms:Math.round(performance.now()-start)};
}
const results=await Promise.all(jobs.map(one));
const latest=Math.max(...results.map(x=>x.start_ms));
const earliest=Math.min(...results.map(x=>x.end_ms));
console.log(JSON.stringify({results,overlap:earliest>latest},null,2));
if(!(earliest>latest)) throw new Error("requests did not overlap");
console.log("MULTIPROCESS_2WAY_ACCEPTANCE=PASS");
