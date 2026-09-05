// Development-only oracle for the still-open lossless UTF-16 storage gate.
// resultJson is a JSON string so the fixture itself remains valid UTF-8 JSON.
import {execFileSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const load=(name:string)=>import(pathToFileURL(`${root}/${name}`).href);
const {createGrepToolDefinition}=await load('packages/coding-agent/src/core/tools/grep.ts');
const {convertResponsesMessages}=await load('packages/ai/src/api/openai-responses-shared.ts');
const {sanitizeSurrogates}=await load('packages/ai/src/utils/sanitize-unicode.ts');
const cwd=mkdtempSync(join(tmpdir(),'pi-surrogate-'));
const model={id:'fixture',name:'Fixture',provider:'openai-codex',api:'openai-codex-responses',baseUrl:'https://example.invalid',reasoning:true,input:['text'],contextWindow:8192,maxTokens:1024,cost:{input:0,output:0,cacheRead:0,cacheWrite:0}};
const cases:any[]=[];
try {
 for(const prefix of ['marker','markerX']) {
  const file=prefix+'🙂'.repeat(300);
  writeFileSync(join(cwd,'file.txt'),file);
  const args={pattern:'marker'};
  const result=await createGrepToolDefinition(cwd).execute('fixture',args,undefined,undefined,{cwd});
  const text=result.content[0].text;
  const call={type:'toolCall',id:'call|fc_tool',name:'grep',arguments:args};
  const assistant={role:'assistant',content:[call],provider:model.provider,api:model.api,model:model.id,stopReason:'toolUse',timestamp:1,usage:{input:0,output:0,cacheRead:0,cacheWrite:0,totalTokens:0,cost:{input:0,output:0,cacheRead:0,cacheWrite:0,total:0}}};
  const message={role:'toolResult',toolCallId:call.id,toolName:'grep',content:result.content,isError:false,timestamp:2};
  const providerItems=convertResponsesMessages(model,{messages:[assistant,message]},new Set(['openai-codex']));
  cases.push({name:prefix==='marker'?'paired-boundary':'split-surrogate-boundary',file,args,resultJson:JSON.stringify(result),textCodeUnits:Array.from({length:text.length},(_,i)=>text.charCodeAt(i)),sanitizedText:sanitizeSurrogates(text),providerItems});
 }
 writeFileSync(process.argv[2],JSON.stringify({pin,status:'Known open raw UTF-16 preservation gate; not included in the passing tool fixture count.',cases},null,2)+'\n');
 console.log(`Captured ${cases.length} raw tool and provider conversion cases.`);
} finally {rmSync(cwd,{recursive:true,force:true});}
