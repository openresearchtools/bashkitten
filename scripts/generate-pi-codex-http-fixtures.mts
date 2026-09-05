// Actual pinned Codex SSE transport with mocked fetch; no network.
import {execFileSync} from 'node:child_process';import {writeFileSync} from 'node:fs';import {pathToFileURL} from 'node:url';import {zstdDecompressSync} from 'node:zlib';import {STATUS_CODES} from 'node:http';
const root=process.env.PI_REFERENCE!;const pin='9841914c71a74d81abe07f751aefd271fd924e63';if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
globalThis.fetch=async()=>{throw Error('No network');};
const {stream}=await import(pathToFileURL(`${root}/packages/ai/src/api/openai-codex-responses.ts`).href);
const model={id:'gpt-5.5',name:'Fixture',api:'openai-codex-responses',provider:'openai-codex',baseUrl:'http://localhost/backend-api',contextWindow:272000,maxTokens:128000,reasoning:true,input:['text','image'],cost:{input:0,output:0,cacheRead:0,cacheWrite:0}};
const token='e30.'+Buffer.from(JSON.stringify({'https://api.openai.com/auth':{chatgpt_account_id:'fixture-account'}})).toString('base64url')+'.fixture';
const cases:any[]=[];const ok={status:200,body:'data: {"type":"response.completed","response":{"id":"response-fixture","status":"completed","output":[],"usage":{"input_tokens":100,"output_tokens":10,"total_tokens":110}}}\n\n'};
async function add(name:string,responses:any[],options:any={},extra:any={}){
 const requests:any[]=[];const result=stream({...model,...extra},{messages:[{role:'user',content:'test',timestamp:0}]},{apiKey:token,sessionId:'session-'+ 'x'.repeat(80),transport:'sse',maxRetries:0,...options,fetch:async(url:any,init:any)=>{
  const headers=new Headers(init.headers);const decoded=headers.get('content-encoding')==='zstd'?zstdDecompressSync(init.body).toString():String(init.body);
  requests.push({url,headers:Object.fromEntries(headers),body:JSON.parse(decoded),compressed:headers.get('content-encoding')==='zstd'?Buffer.from(init.body).toString('base64'):null});
  const response=responses[Math.min(requests.length-1,responses.length-1)];return new Response(response.body,{status:response.status,statusText:response.statusText??STATUS_CODES[response.status],headers:{'content-type':response.status===200?'text/event-stream':'application/json',...response.headers}});
 }});for await(const _ of result){}const message=await result.result();cases.push({name,responses,options,model:{...model,...extra},requests,expectedError:message.errorMessage??null,expectedStop:message.stopReason});
}
for(const [status,body] of [[400,JSON.stringify({error:{message:'bad input',type:'invalid_request_error',param:'messages'}})],[429,JSON.stringify({error:{message:'Quota exhausted',code:'insufficient_quota'}})],[429,JSON.stringify({error:{code:'usage_limit_reached',plan_type:'PLUS',resets_at:1}})],[503,'upstream unavailable'],[401,''],[422,JSON.stringify({detail:'Invalid',message:'Top level error'})],[400,JSON.stringify({error:'plain string'})],[400,JSON.stringify({error:{message:{field:'bad'}}})],[500,JSON.stringify({error:{message:'  preserve\nspaces  '}})],[400,JSON.stringify({error:{message:'x'.repeat(5000)}})]])await add(`http-${status}-${cases.length}`,[{status,body}]);
for(const status of [400,408,429,500,501,503])await add(`retry-${status}`,[{status,body:'temporary',headers:{'retry-after-ms':'0'}},ok],{maxRetries:1});
await add('terminal-quota',[{status:429,body:JSON.stringify({error:{code:'insufficient_quota',message:'Quota exhausted'}})},ok],{maxRetries:2});
await add('ignored-sdk-retry-header',[{status:503,body:'no retry',headers:{'x-should-retry':'false','retry-after-ms':'0'}},ok],{maxRetries:1});
await add('retry-delay-cap',[{status:429,body:'wait',headers:{'retry-after-ms':'61000'}}],{maxRetries:1});
await add('retry-limit',[{status:503,body:'overloaded',headers:{'retry-after-ms':'0'}}],{maxRetries:2});
for(const cacheRetention of ['none','short','long'])await add(`cache-${cacheRetention}`,[ok],{cacheRetention});
await add('options-headers',[ok],{headers:{Authorization:'Bearer ignored','session-id':'override-session','x-fixture':'option','User-Agent':'ignored'}},{headers:{'x-fixture':'model'}});
for(const baseUrl of ['http://localhost/backend-api/codex','http://localhost/backend-api/codex/responses///'])await add('url-'+baseUrl,[ok],{},{baseUrl});
writeFileSync(process.argv[2],JSON.stringify({pin,token,cases},null,2)+'\n');console.log(`Captured ${cases.length} Codex HTTP/retry/header cases without network.`);
