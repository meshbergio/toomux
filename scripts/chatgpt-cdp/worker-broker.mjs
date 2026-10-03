#!/usr/bin/env node
import http from "node:http";

const HOST=process.env.TOOMUX_CHATGPT_BROKER_HOST||"127.0.0.1";
const PORT=Number(process.env.TOOMUX_CHATGPT_BROKER_PORT||34561);
const WORKERS=(process.env.TOOMUX_CHATGPT_WORKERS||"http://127.0.0.1:34581,http://127.0.0.1:34582").split(",").map(x=>x.trim()).filter(Boolean);
const MAX_BODY=12*1024*1024;
const leases=new Map();
const workerState=WORKERS.map((base,i)=>({id:`worker-${i+1}`,base,sessions:new Set(),active:0,queued:0,lastUsedAt:0,healthy:true,effort:null}));

function json(res,status,body){const data=Buffer.from(JSON.stringify(body));res.writeHead(status,{"content-type":"application/json; charset=utf-8","content-length":data.length,"cache-control":"no-store"});res.end(data)}
async function readBody(req){const chunks=[];let n=0;for await(const c of req){n+=c.length;if(n>MAX_BODY){const e=new Error("request body too large");e.status=413;throw e}chunks.push(c)}return Buffer.concat(chunks)}
function sid(body,req){return String(body?.provider_session_id||req.headers["x-toomux-provider-session-id"]||"").trim()}
function view(w){return{id:w.id,base:w.base,sessions:[...w.sessions],lease_count:w.sessions.size,active:w.active,queued:w.queued,lastUsedAt:w.lastUsedAt,healthy:w.healthy,effort:w.effort}}
function score(w,effort){return w.active*1000+w.queued*100+w.sessions.size*10+(effort&&w.effort!==effort?5:0)}
async function health(w,{deep=false}={}){try{const root=w.base.replace(/\/v1\/?$/,"");const path=deep&&w.active===0?"/readyz":"/health";const timeout=path==="/readyz"?7000:1500;const r=await fetch(root+path,{signal:AbortSignal.timeout(timeout)});const b=await r.json().catch(()=>({}));w.healthy=r.ok&&b.ok!==false;const observed=String(b.reasoning_effort||b?.lanes?.[0]?.effort||"").toLowerCase();if(observed)w.effort=observed;return w.healthy}catch{w.healthy=false;return false}}
async function pick(sessionId,effort=""){
  if(sessionId&&leases.has(sessionId)){const w=workerState.find(x=>x.id===leases.get(sessionId));if(w&&await health(w))return w;if(w)w.sessions.delete(sessionId);leases.delete(sessionId)}
  await Promise.all(workerState.map(health));
  const candidates=workerState.filter(w=>w.healthy);
  if(!candidates.length)throw new Error("no healthy ChatGPT browser workers");
  candidates.sort((a,b)=>score(a,effort)-score(b,effort)||a.lastUsedAt-b.lastUsedAt);
  const w=candidates[0];
  if(sessionId){w.sessions.add(sessionId);leases.set(sessionId,w.id)}
  return w;
}
async function proxy(req,res,body){
  const parsed=body.length?JSON.parse(body.toString("utf8")):{};
  const sessionId=sid(parsed,req);
  const effort=String(parsed?.reasoning_effort||"").toLowerCase();
  const worker=await pick(sessionId,effort);
  worker.active+=1;worker.lastUsedAt=Date.now();
  try{
    const target=worker.base+"/v1/chat/completions";
    const headers={"content-type":"application/json"};
    if(req.headers.authorization)headers.authorization=String(req.headers.authorization);
    const workerBody=Buffer.from(JSON.stringify({...parsed,provider_session_id:undefined}));
    const upstream=await fetch(target,{method:"POST",headers,body:workerBody,signal:req.signal});
    res.writeHead(upstream.status,Object.fromEntries([...upstream.headers].filter(([k])=>!["content-length","content-encoding","transfer-encoding","connection"].includes(k.toLowerCase()))));
    if(upstream.body){for await(const chunk of upstream.body)res.write(chunk)}
    res.end();
  } finally {worker.active-=1;worker.lastUsedAt=Date.now()}
}
const server=http.createServer(async(req,res)=>{
  try{
    const u=new URL(req.url||"/",`http://${HOST}:${PORT}`);
    if(req.method==="GET"&&u.pathname==="/health")return json(res,200,{ok:true,service:"toomux-chatgpt-worker-broker",workers:workerState.map(view),leases:leases.size});
    if(req.method==="GET"&&u.pathname==="/readyz"){await Promise.all(workerState.map(w=>health(w,{deep:true})));const ok=workerState.some(w=>w.healthy);return json(res,ok?200:503,{ok,workers:workerState.map(view),leases:leases.size})}
    if(req.method==="GET"&&u.pathname==="/v1/models"){
      const w=await pick("");
      const r=await fetch(w.base+"/v1/models",{headers:req.headers.authorization?{authorization:String(req.headers.authorization)}:{}});
      const t=await r.text();res.writeHead(r.status,{"content-type":r.headers.get("content-type")||"application/json","cache-control":"no-store"});return res.end(t)
    }
    if(req.method!=="POST"||u.pathname!=="/v1/chat/completions")return json(res,404,{error:{message:"not found",type:"invalid_request_error"}});
    const body=await readBody(req);
    return await proxy(req,res,body);
  }catch(e){if(!res.headersSent)json(res,Number(e?.status)||502,{error:{message:String(e?.message||e),type:"server_error"}});else try{res.end()}catch{}}
});
server.listen(PORT,HOST,()=>process.stdout.write(`toomux ChatGPT worker broker listening on http://${HOST}:${PORT} workers=${WORKERS.length}\n`));
