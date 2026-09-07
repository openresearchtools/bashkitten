// Actual pinned OpenAI OAuth refresh implementation; synthetic credentials only.
import {execFileSync} from 'node:child_process';import {writeFileSync} from 'node:fs';import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong pin');
const {openaiCodexOAuth}=await import(pathToFileURL(`${root}/packages/ai/src/auth/oauth/openai-codex.ts`).href);
const token=(account:string)=>'e30.'+Buffer.from(JSON.stringify({'https://api.openai.com/auth':{chatgpt_account_id:account}})).toString('base64')+'.fixture';
const cases=[];const now=1700000000000;Date.now=()=>now;
for(const spec of [
 {name:'rotated-account',body:{access_token:token('new-account'),refresh_token:'new-refresh',expires_in:3600}},
 ...[0,-1,.001].map(expires_in=>({name:'expiry-'+expires_in,body:{access_token:token('fixture-account'),refresh_token:'new-refresh',expires_in}})),
 {name:'invalid-access',body:{access_token:'invalid',refresh_token:'new-refresh',expires_in:3600}},
 {name:'missing-refresh',body:{access_token:token('fixture-account'),expires_in:3600}},
 {name:'missing-expiry',body:{access_token:token('fixture-account'),refresh_token:'new-refresh'}},
 {name:'http-failure',status:400,raw:'invalid_grant'},
 {name:'malformed',raw:'{broken'},
] as any[]){let request:any;globalThis.fetch=async(url,init)=>{request={url,method:init?.method,body:String(init?.body)};return new Response(spec.raw??JSON.stringify(spec.body),{status:spec.status??200,headers:{'content-type':'application/json'}});};let expected:any;try{expected={credential:await openaiCodexOAuth.refresh({type:'oauth',access:token('old-account'),refresh:'old-refresh',expires:0,accountId:'old-account'},new AbortController().signal)}}catch(error){expected={error:(error as Error).message};}cases.push({spec,request,expected});}
writeFileSync(process.argv[2],JSON.stringify({pin,now,cases},null,2)+'\n');console.log(`Captured ${cases.length} refresh outcomes.`);
