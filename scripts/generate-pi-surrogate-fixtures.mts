// Development-only oracle for lossless UTF-16 logical history and provider boundaries.
// resultJson is a JSON string so the fixture itself remains valid UTF-8 JSON.
import {execFileSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,readFileSync,mkdirSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const load=(name:string)=>import(pathToFileURL(`${root}/${name}`).href);
const {createGrepToolDefinition}=await load('packages/coding-agent/src/core/tools/grep.ts');
const {convertResponsesMessages}=await load('packages/ai/src/api/openai-responses-shared.ts');
const {sanitizeSurrogates}=await load('packages/ai/src/utils/sanitize-unicode.ts');
const {serializeConversation}=await load('packages/coding-agent/src/core/compaction/utils.ts');
const {estimateTokens}=await load('packages/coding-agent/src/core/compaction/compaction.ts');
const toolFactory:any={};
for(const name of ['read','write','edit','grep','find','ls','bash']){
 const mod=await load(`packages/coding-agent/src/core/tools/${name}.ts`);
 toolFactory[name]=mod[`create${name[0].toUpperCase()+name.slice(1)}ToolDefinition`];
}
const {validateToolArguments}=await load('packages/ai/src/utils/validation.ts');
const cwd=mkdtempSync(join(tmpdir(),'pi-surrogate-'));
const model={id:'fixture',name:'Fixture',provider:'openai-codex',api:'openai-codex-responses',baseUrl:'https://example.invalid',reasoning:true,input:['text'],contextWindow:8192,maxTokens:1024,cost:{input:0,output:0,cacheRead:0,cacheWrite:0}};
const cases:any[]=[];
try {
 for(const prefix of ['marker','markerX','bulk']) {
  const file=prefix==='bulk'?('markerX'+'🙂'.repeat(300)+'\n').repeat(100):prefix+'🙂'.repeat(300);
  writeFileSync(join(cwd,'file.txt'),file);
  const args={pattern:'marker'};
  const result=await createGrepToolDefinition(cwd).execute('fixture',args,undefined,undefined,{cwd});
  const text=result.content[0].text;
  const call={type:'toolCall',id:'call|fc_tool',name:'grep',arguments:args};
  const assistant={role:'assistant',content:[call],provider:model.provider,api:model.api,model:model.id,stopReason:'toolUse',timestamp:1,usage:{input:0,output:0,cacheRead:0,cacheWrite:0,totalTokens:0,cost:{input:0,output:0,cacheRead:0,cacheWrite:0,total:0}}};
  const message={role:'toolResult',toolCallId:call.id,toolName:'grep',content:result.content,isError:false,timestamp:2};
  const providerItems=convertResponsesMessages(model,{messages:[assistant,message]},new Set(['openai-codex']));
  cases.push({name:prefix==='marker'?'paired-boundary':prefix==='bulk'?'split-surrogates-byte-truncation':'split-surrogate-boundary',file,args,resultJson:JSON.stringify(result),textCodeUnits:prefix==='bulk'?undefined:Array.from({length:text.length},(_,i)=>text.charCodeAt(i)),sanitizedText:sanitizeSurrogates(text),providerItems,model,assistant,messageJson:JSON.stringify(message),estimatedTokens:estimateTokens(message)});
 }
 const toolCases:any[]=[];
 const invocations=[
 ['write-unknown-lone-key','write',{path:'written.txt',content:'ok','\ud800':1,'$bashkitten.internal.utf16':[55296]},{}],
 ['edit-unknown-lone-key','edit',{path:'file.txt',oldText:'x',newText:'\ud800','\udc00':2},{'file.txt':'x'}],
 ['edit-stringified-lone','edit',{path:'file.txt',edits:'[{"oldText":"x","newText":"'+String.fromCharCode(0xd800)+'"}]'},{'file.txt':'x'}],
 ['bash-inspect-split','bash',{command:'\0'+'a'.repeat(122)+'🙂'.repeat(50)},{}],
 ['bash-lone-nul','bash',{command:'\0\ud800'},{}],
 ['grep-lone-nul','grep',{pattern:'\0\ud800'},{}],
 ['find-lone-nul','find',{pattern:'\0\ud800'},{}],
 ['read-lone-nul','read',{path:'x\0\ud800'},{}],
 ['edit-lone-path','edit',{path:'missing-\ud800',oldText:'x',newText:'y'},{}],
 ['edit-alphabet-collision','edit',{path:'file.txt',oldText:'\ud83d',newText:'x'},{'file.txt':'\u{f0000}🙂'}],
 ['edit-nfkc-lone','edit',{path:'file.txt',oldText:'A',newText:'a\ud800'},{'file.txt':'𝐀 text'}],
 ['write-lone-content','write',{path:'written.txt',content:'a\ud800b\udc00c'},{}],
 ['write-lone-path','write',{path:'file-\ud800.txt',content:'ok'},{}],
 ['read-lone-path','read',{path:'missing-\ud800.txt'},{}],
 ['ls-lone-path','ls',{path:'missing-\ud800'},{}],
 ['find-lone-pattern','find',{pattern:'*\ud800*'},{'file-�.txt':'ok'}],
 ['grep-lone-pattern','grep',{pattern:'\ud800'},{'file.txt':'a�b'}],
 ['bash-lone-command','bash',{command:"printf 'a\ud800b'"},{}],
 ['edit-new-lone','edit',{path:'file.txt',oldText:'before',newText:'a\ud800b'},{'file.txt':'before'}],
 ['edit-old-lone-no-match','edit',{path:'file.txt',oldText:'\ud800',newText:'x'},{'file.txt':'�'}],
 ['edit-old-half-pair','edit',{path:'file.txt',oldText:'\ud83d',newText:'x'},{'file.txt':'🙂'}],
 ['validation-lone-string','read',{path:'x\ud800',offset:{}},{}],
 ['validation-lone-as-object','read','\ud800',{}],
 ];
 for(const [name,tool,args,files] of invocations){
  const dir=mkdtempSync(join(tmpdir(),'pi-surrogate-tools-'));
  try {
   if(tool==='find')mkdirSync(join(dir,'.git'));
   for(const [file,content] of Object.entries(files))writeFileSync(join(dir,file),content as string);
   const definition=toolFactory[tool as string](dir);
   const call={type:'toolCall',id:'fixture',name:tool,arguments:args};
   let result:any;
   try {if(definition.prepareArguments)call.arguments=definition.prepareArguments(call.arguments);const prepared=validateToolArguments(definition,call);result={result:await definition.execute('fixture',prepared,undefined,undefined,{cwd:dir,sessionManager:{getSessionId:()=>undefined,getSessionFile:()=>undefined}})};}catch(error:any){result={error:error.message};}
   const changed:any={};
   for(const file of ['file.txt','written.txt','file-�.txt']){try{changed[file]=readFileSync(join(dir,file),'utf8');}catch{}}
   toolCases.push({name,tool,dirs:tool==='find'?['.git']:[],argsJson:JSON.stringify(args),files,resultJson:JSON.stringify(result).replaceAll(dir,'<ROOT>'),changed});
  }finally{rmSync(dir,{recursive:true,force:true});}
 }
 const summaryCases = ['x'.repeat(1999)+'🙂end', 'x'.repeat(1998)+'🙂end'].map((text,index)=>{
  const message={role:'toolResult',toolCallId:'call',toolName:'grep',content:[{type:'text',text}],isError:false,timestamp:0};
  const summary=serializeConversation([message]);
  return {name:index===0?'summary-split-pair':'summary-paired-boundary',messageJson:JSON.stringify(message),summaryJson:JSON.stringify(summary),summaryCodeUnits:Array.from({length:summary.length},(_,i)=>summary.charCodeAt(i)),sanitizedSummary:sanitizeSurrogates(summary)};
 });
 const summaryArgs=JSON.parse(String.raw`{"$bashkitten.internal.utf16":[55296],"\ud800":"\udc00","nested":{"$bashkitten.internal.object":[["x",3]]}}`);
 const summaryMessage={...cases[0].assistant,content:[{type:'toolCall',id:'call',name:'write',arguments:summaryArgs}]};
 const argumentSummary=serializeConversation([summaryMessage]);
 summaryCases.push({name:'summary-lone-and-reserved-argument-keys',messageJson:JSON.stringify(summaryMessage),summaryJson:JSON.stringify(argumentSummary),summaryCodeUnits:Array.from({length:argumentSummary.length},(_,i)=>argumentSummary.charCodeAt(i)),sanitizedSummary:sanitizeSurrogates(argumentSummary)});
 const serializationCases = [String.raw`["\ud800x\udc00","🙂","\ud83d\ude42"]`,String.raw`{"$bashkitten.internal.utf16":[55296],"nested":{"$bashkitten.internal.object":[["x",3]]}}`,String.raw`{"\ud800":"\udc00","normal":{"$bashkitten.internal.utf16":[55296]}}`,String.raw`{"\ud800":1,"\ud800":2,"x":3}`,String.raw`{"b":1,"1":2,"a":3,"0":4}`, '{"x":"'+String.fromCharCode(0xd800)+'"}'].map(input=>({inputJson:JSON.stringify(input),expectedJson:JSON.stringify(JSON.parse(input))}));
 writeFileSync(process.argv[2],JSON.stringify({pin,status:'Pinned UTF-16 tool, history, compaction and provider differential fixtures.',cases,toolCases,summaryCases,serializationCases},null,2)+'\n');
 console.log(`Captured ${cases.length} raw tool and provider conversion cases.`);
} finally {rmSync(cwd,{recursive:true,force:true});}
