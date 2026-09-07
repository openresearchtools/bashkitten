// Pinned Pi's actual SSE transport, using only deterministic fetch responses.
import {execFileSync} from 'node:child_process';
import {writeFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong pin');
globalThis.fetch=async()=>{throw Error('No network');};
const {stream}=await import(pathToFileURL(`${root}/packages/ai/src/api/openai-codex-responses.ts`).href);
const token='e30.'+Buffer.from(JSON.stringify({'https://api.openai.com/auth':{chatgpt_account_id:'fixture-account'}})).toString('base64url')+'.fixture';
const model={id:'gpt-5.5',name:'Fixture',api:'openai-codex-responses',provider:'openai-codex',baseUrl:'http://localhost/backend-api',contextWindow:272000,maxTokens:128000,reasoning:true,input:['text','image'],cost:{input:1,output:2,cacheRead:.1,cacheWrite:1.25}};
const inputs=['😀','{"a":😀}','x'.repeat(9)+'😀'+'x'.repeat(30),'','{','[','{"a"','{"a":','{"a":1','{"a":1,','{broken','{a:1}','{"a":}','[1,]','[1 2]','{"a" 1}','{"a":1 "b":2}','"abc','"a\\','"a\\x"','"\\u1"','"a\nb"','true false','undefined','NaN','Infinity','-','01','1.','1e','1e+','[.1]','tru','trux','nulx','truex','falsex','nullx','{"😀": false x}','{"a": \n false x}','[}','{]','[1:2]','{"a":01}',' [1]\rX','n','{"a": true, "b": [1, 2, 3, x]}','x'.repeat(100),'["'+'a'.repeat(50)+'",x]',' '.repeat(50)+'X','true\r\n x','{"a":\r\n }','-x','-01','1.e','[1','[1,','"\\u','"\\u123','"\t"','[object Object]','[undefined]'];
for(const before of [0,1,8,9,10,11,15,20,30])for(const after of [0,5,9,10,11,20]) inputs.push(' '.repeat(before)+'X'+' '.repeat(after));
const cases=[];
for(const input of inputs){let expected='';try{JSON.parse(input);throw Error('Expected malformed input');}catch(error){expected=(error as Error).message;}
 // Each data line is trimmed by pinned Pi. Capture the actual stream error
 // separately so framing and prior-output retention are compared as well.
 const prefix='data: '+JSON.stringify({type:'response.output_item.done',output_index:0,item:{type:'message',id:'msg_1',role:'assistant',phase:'final_answer',content:[{type:'output_text',text:'Retained',annotations:[]}]}})+'\n\n';
 const payload=prefix+input.split('\n').map(line=>'data: '+line).join('\n')+'\n\n';
 const result=stream(model,{messages:[]},{apiKey:token,transport:'sse',fetch:async()=>new Response(payload,{headers:{'content-type':'text/event-stream'}})});
 for await(const event of result){} const output=await result.result();delete output.timestamp;
 cases.push({input,expected,payload,output});
}
writeFileSync(process.argv[2],JSON.stringify({pin,node:process.version,v8:process.versions.v8,token,model,cases},null,2)+'\n');
console.log(`Captured ${cases.length} malformed JSON and complete Pi SSE outcomes.`);
