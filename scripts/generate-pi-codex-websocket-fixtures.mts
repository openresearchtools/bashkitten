// Execute pinned WebSocket continuation matching; no network or generated model catalog.
import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,unlinkSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong pin');
globalThis.fetch=async()=>{throw Error('No network');};
const path=`${root}/packages/ai/src/api/.fixture-codex-websocket.ts`;
writeFileSync(path,readFileSync(`${root}/packages/ai/src/api/openai-codex-responses.ts`,'utf8')+'\nexport {buildCachedWebSocketRequestBody,buildWebSocketHeaders,resolveCodexWebSocketUrl};\n');
try {
const {buildCachedWebSocketRequestBody,buildWebSocketHeaders,resolveCodexWebSocketUrl}=await import(pathToFileURL(path).href);
const input=[{role:'user',content:[{type:'input_text',text:'First'}]}],output=[{type:'message',role:'assistant',id:'msg_1',content:[{type:'output_text',text:'Answer',annotations:[]}],status:'completed'}],next={role:'user',content:[{type:'input_text',text:'Next'}]};
const body={model:'gpt-5.5',store:false,stream:true,instructions:'System',input,tools:[],tool_choice:'auto',parallel_tool_calls:true};
const continuation={lastRequestBody:body,lastResponseId:'response1',lastResponseItems:output};
const cases:any[]=[];
function add(name:string,body:any,previous:any=continuation){const entry={continuation:structuredClone(previous)};const expected=buildCachedWebSocketRequestBody(entry,body);cases.push({name,body,continuation:previous,expected,retained:entry.continuation!=null,serialized:JSON.stringify(expected)});}
add('first',body,null);add('delta',{...body,input:[...input,...output,next]});add('empty-delta',{...body,input:[...input,...output]});add('short',{...body,input});add('changed-instructions',{...body,instructions:'Changed',input:[...input,...output,next]});add('changed-input',{...body,input:[next,...output,next]});add('missing-id',{...body,input:[...input,...output,next]},{...continuation,lastResponseId:''});add('existing-previous',{...body,previous_response_id:'wrong',input:[...input,...output,next]});add('reordered-body',{input:[...input,...output,next],...body,model:'gpt-5.5'});add('reordered-item',{...body,input:[...input,{id:output[0].id,...output[0]},next]});add('no-input',{model:'gpt-5.5'},{lastRequestBody:{model:'gpt-5.5'},lastResponseId:'empty',lastResponseItems:[]});
const headers=[];for(const modelHeaders of [{},{accept:'bad','content-type':'bad','openai-beta':'bad','x-test':'model'}])for(const options of [{},{'x-test':'option',authorization:'wrong','chatgpt-account-id':'wrong'},{'x-test':null}])headers.push({modelHeaders,options,expected:Object.fromEntries(buildWebSocketHeaders(modelHeaders,options,'account','token','session'))});
const urls=['','http://localhost/backend-api','https://host/codex','https://host/codex/responses','https://host/codex/responses///','http://LOCALHOST:80/backend-api','https://host:443/base path',' https://host/base ','https://münich.example/模型'].map(base=>({base,expected:resolveCodexWebSocketUrl(base)}));
writeFileSync(process.argv[2],JSON.stringify({pin,cases,headers,urls},null,2)+'\n');
console.log(`Captured ${cases.length} continuation cases, ${headers.length} headers and ${urls.length} URLs.`);
} finally {unlinkSync(path);}
