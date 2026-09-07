// Actual pinned transport state machine with a deterministic, event-compatible socket.
import {execFileSync} from 'node:child_process';import {readFileSync,writeFileSync} from 'node:fs';import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong pin');
const model=JSON.parse(readFileSync('reference/openai-codex-models.json','utf8')).find(m=>m.id==='gpt-5.5');model.baseUrl='http://localhost/backend-api';
const token='e30.'+Buffer.from(JSON.stringify({'https://api.openai.com/auth':{chatgpt_account_id:'fixture-account'}})).toString('base64url')+'.fixture';
let plans:any[]=[],requests:any[]=[],connections=0;
class FakeSocket {
 readyState=0;listeners=new Map<string,Set<Function>>();
 constructor(_url:any,_options:any){connections++;setTimeout(()=>{this.readyState=1;this.emit('open',{});},1);}
 addEventListener(kind:string,fn:Function){if(!this.listeners.has(kind))this.listeners.set(kind,new Set());this.listeners.get(kind)!.add(fn);}
 removeEventListener(kind:string,fn:Function){this.listeners.get(kind)?.delete(fn);}
 emit(kind:string,event:any){for(const fn of this.listeners.get(kind)||[])fn(event);}
 send(data:string){requests.push(JSON.parse(data));const plan=plans.shift();if(!plan)throw Error('Missing socket plan');void(async()=>{for(const frame of plan){await new Promise(r=>setTimeout(r,2));if(this.readyState!==1)return;if(frame.wait)continue;if(frame.close){this.readyState=3;this.emit('close',{...frame.close,wasClean:true});return;}this.emit('message',{data:frame.raw??JSON.stringify(frame)});}})();}
 close(code=1000,reason='done'){this.readyState=3;this.emit('close',{code,reason,wasClean:true});}
}
(globalThis as any).WebSocket=FakeSocket;
const {stream,closeOpenAICodexWebSocketSessions,resetOpenAICodexWebSocketDebugStats}=await import(pathToFileURL(`${root}/packages/ai/src/api/openai-codex-responses.ts`).href);
function success(id:string){return [{type:'response.created',response:{id}},{type:'response.output_item.done',output_index:0,item:{type:'message',id:'msg_'+id,role:'assistant',phase:'final_answer',content:[{type:'output_text',text:'Answer '+id,annotations:[]}]}},{type:'response.completed',response:{id,status:'completed',output:[]}}];}
const error=(code:string)=>({type:'error',code,message:code});const closed={close:{code:1011,reason:'fixture'}};const created={type:'response.created',response:{id:'partial'}};
const specs:any[]=[
 {name:'malformed-before-start',plans:[[{raw:'{broken'}],success('two')],turns:2},
 {name:'malformed-after-start',plans:[[created,{raw:'{"a":}' }],success('two')],turns:2},
 {name:'null-json-before-start',plans:[[{raw:'null'}],success('two')],turns:2},
 {name:'auto-continuation',plans:[success('one'),success('two'),success('three')],turns:3},
 {name:'changed-instructions',plans:[success('one'),success('two')],turns:2,changed:true},
 {name:'websocket-full',transport:'websocket',plans:[success('one'),success('two')],turns:2},
 {name:'cached-explicit',transport:'websocket-cached',plans:[success('one'),success('two')],turns:2},
 {name:'default-options-full',absent:true,plans:[success('one'),success('two')],turns:2},
 {name:'cache-none',none:true,plans:[success('one'),success('two')],turns:2},
 {name:'fallback-before-start',plans:[[closed]],turns:2},
 {name:'failure-after-start',plans:[[created,closed]],turns:2},
 {name:'api-error',plans:[[error('invalid_request_error')],success('two')],turns:2},
 {name:'missing-continuation-retry',plans:[[error('previous_response_not_found')],success('one')],turns:1},
 {name:'missing-continuation-twice',plans:[[error('previous_response_not_found')],[error('previous_response_not_found')]],turns:1},
 {name:'connection-limit-retry',plans:[[error('websocket_connection_limit_reached')],success('one')],turns:1},
 {name:'connection-limit-twice',plans:[[error('websocket_connection_limit_reached')],[error('websocket_connection_limit_reached')]],turns:1},
 {name:'connection-limit-after-start',plans:[[created,error('websocket_connection_limit_reached')]],turns:1},
 {name:'missing-after-start',plans:[[created,error('previous_response_not_found')],success('one')],turns:1},
 {name:'terminal-error-fallback-next',plans:[[{type:'response.completed',response:{id:'bad',status:'incomplete',incomplete_details:{reason:'content_filter'},output:[]}}]],turns:2},
 {name:'no-type-before-close',plans:[[{},closed]],turns:1},
 {name:'idle-before-start',plans:[[{wait:true}]],timeoutMs:20,turns:1},
 {name:'idle-after-start',plans:[[created,{wait:true}]],timeoutMs:20,turns:1},
];
const cases=[];
function normalized(value:any):any {if(Array.isArray(value))return value.map(normalized);if(value&&typeof value==='object'){const out:any={};for(const [key,v] of Object.entries(value))if(key!=='timestamp'&&key!=='stack')out[key]=normalized(v);return out;}return value;}
for(const spec of specs){closeOpenAICodexWebSocketSessions();resetOpenAICodexWebSocketDebugStats();plans=structuredClone(spec.plans);requests=[];connections=0;let sse=0;const outputs=[],messages:any[]=[];let fetchRequests:any[]=[];
 for(let turn=0;turn<spec.turns;turn++){messages.push({role:'user',content:'Question '+turn,timestamp:turn});const options:any={apiKey:token,sessionId:'fixture-'+spec.name,transport:spec.absent?undefined:spec.transport||'auto',cacheRetention:spec.none?'none':undefined,timeoutMs:spec.timeoutMs,fetch:async(_url:any,init:any)=>{sse++;const {zstdDecompressSync}=await import('node:zlib');let bytes=init.body;const headers=new Headers(init.headers);if(headers.get('content-encoding')==='zstd')bytes=zstdDecompressSync(bytes);fetchRequests.push(JSON.parse(Buffer.from(bytes).toString()));return new Response(success('sse'+sse).map(v=>'data: '+JSON.stringify(v)+'\n\n').join(''),{headers:{'content-type':'text/event-stream'}});}};
 const result=stream(model,{systemPrompt:spec.changed&&turn?'Changed':'System',messages},options);const events=[];for await(const event of result)events.push(event.type);const output=await result.result();outputs.push({message:normalized(output),events});messages.push(output);
 }
 cases.push({spec,outputs,requests,fetchRequests,connections});}
closeOpenAICodexWebSocketSessions();writeFileSync(process.argv[2],JSON.stringify({pin,model,token,cases},null,2)+'\n');console.log(`Captured ${cases.length} complete transport scenarios.`);
