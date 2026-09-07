// Offline oracle: execute the pinned Codex builder, shared conversion and catalog metadata.
import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,unlinkSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!;const pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
globalThis.fetch=async()=>{throw Error('No network in Codex fixtures');};
const requestFile=`${root}/packages/ai/src/api/.fixture-codex-requests.ts`;
const catalogFile=`${root}/packages/ai/scripts/.fixture-codex-catalog.ts`;
const generator=readFileSync(`${root}/packages/ai/scripts/generate-models.ts`,'utf8');
const catalog=generator.slice(generator.indexOf('\tconst CODEX_BASE_URL ='),generator.indexOf('\tallModels.push(...codexModels);'));
writeFileSync(catalogFile,generator.slice(0,generator.indexOf('// Run the generator'))+`\nexport function fixtureCatalog(){${catalog}\n for(const model of codexModels){applyThinkingLevelMetadata(model);applyStrictToolCompatMetadata(model);applyOpenAIGrammarToolCompatMetadata(model);applyOpenAIToolSearchMetadata(model);}return codexModels;}\n`);
writeFileSync(requestFile,readFileSync(`${root}/packages/ai/src/api/openai-codex-responses.ts`,'utf8')+'\nexport {buildRequestBody};\n');
try{
const {buildRequestBody}=await import(pathToFileURL(requestFile).href);
const args=process.argv;process.argv=process.argv.slice(0,2);
const {fixtureCatalog}=await import(pathToFileURL(catalogFile).href);process.argv=args;
const {clampThinkingLevel,getSupportedThinkingLevels}=await import(pathToFileURL(`${root}/packages/ai/src/models.ts`).href);
const {clampOpenAIPromptCacheKey}=await import(pathToFileURL(`${root}/packages/ai/src/api/openai-prompt-cache.ts`).href);
const models=fixtureCatalog();
const base=models.find(m=>m.id==='gpt-5.5');
const tool={name:'read',description:'Read a file',parameters:{type:'object',properties:{path:{type:'string'}},required:['path']}};
const context={systemPrompt:'  System instructions\n',messages:[{role:'user',content:'Question',timestamp:1}],tools:[tool]};
const cases:any[]=[];
function add(name:string,model:any=base,ctx:any=context,options:any={},simple=false){
 const resolved={...options};
 if(simple){const level=options.reasoning?clampThinkingLevel(model,options.reasoning):undefined;resolved.reasoningEffort=level==='off'?undefined:level;}
 const cacheSessionId=options.cacheRetention==='none'?undefined:clampOpenAIPromptCacheKey(options.sessionId);
 cases.push({name,model,context:ctx,options,simple,expected:buildRequestBody(model,ctx,resolved,cacheSessionId)});
}add('unicode-system',base,{...context,systemPrompt:'\ud83d'});
add('unicode-tool-only',base,{...context,messages:[{role:'assistant',api:'openai-codex-responses',provider:'openai-codex',model:'gpt-5.5',stopReason:'toolUse',timestamp:1,content:[{type:'toolCall',id:'call|fc_call',name:'read',arguments:{path:'a'}}]},{role:'toolResult',toolCallId:'call|fc_call',toolName:'read',content:[{type:'text',text:'\ud83d'}],isError:false,timestamp:2}]});

for(const model of models)for(const reasoning of ['off','minimal','low','medium','high','xhigh','max'])add(`${model.id}-${reasoning}`,model,context,{reasoning,sessionId:'stable'},true);
for(const systemPrompt of ['', ' ', '\n  Keep whitespace \n'])add(`prompt-${JSON.stringify(systemPrompt)}`,base,{...context,systemPrompt});
for(const cacheRetention of ['none','short','long'])for(const sessionId of [undefined,'','a'.repeat(90),'😀'.repeat(70)])add(`cache-${cacheRetention}-${sessionId?.length}`,base,context,{cacheRetention,sessionId});
add('explicit-options',base,context,{textVerbosity:'high',temperature:.25,toolChoice:'required',serviceTier:'priority',reasoningEffort:'high',reasoningSummary:'concise',maxTokens:10,samplingParams:{store:true,stream:false,model:'wrong'}});
for(const map of [{medium:null},{medium:'mapped'},{off:'disabled'}])add(`mapping-${JSON.stringify(map)}`,{...base,thinkingLevelMap:map},context,{reasoningEffort:'medium'});
add('none-mapping',{...base,thinkingLevelMap:{off:'disabled'}},context,{reasoningEffort:'none'});
add('strict-unsupported',{...base,compat:{supportsStrictMode:false}},context);
add('no-tools',base,{...context,tools:[]});
const assistant=(content:any,extra:any={})=>({role:'assistant',content,api:base.api,provider:base.provider,model:base.id,stopReason:'stop',timestamp:1,...extra});
const text={type:'text',text:'Answer'};
const thinking={type:'thinking',thinking:'Thought',thinkingSignature:JSON.stringify({type:'reasoning',id:'rs_1',summary:[{type:'summary_text',text:'Thought'}],encrypted_content:'opaque',status:'completed'})};
const call={type:'toolCall',id:'call|fc_item',name:'read',arguments:{path:'a',nested:{z:1,a:2}},namespace:'functions'};
const result={role:'toolResult',toolCallId:call.id,toolName:'read',content:[{type:'text',text:'output'}],isError:false,timestamp:2};
const image={type:'image',mimeType:'image/png',data:'AQI='};
const histories:any[]=[[],[{role:'user',content:[],timestamp:1},assistant([text])],[assistant([]),assistant([text])],[assistant([thinking,text,call]),result],[assistant([thinking,{...thinking,thinking:'',redacted:true},text,call],{model:'other'}),result],[assistant([thinking,text,call],{api:'openai-responses',provider:'openai'}),result],[assistant([thinking,text,call],{api:'openai-completions',provider:'foreign'}),result],[assistant([call])],[assistant([call]),{role:'user',content:'next'}],[assistant([call]),assistant([text])],[assistant([text,call],{stopReason:'error'}),result],[assistant([text,call],{stopReason:'aborted'})],[assistant([call]),{...result,content:[]}],[assistant([call]),{...result,content:[image,{type:'text',text:'first'},image,{type:'text',text:'second'}]}],[{role:'user',content:[{type:'text',text:'before'},image,image,{type:'text',text:'after'}]}],[{role:'user',content:null},assistant([text])],[assistant([{type:'thinking',thinking:'Unsigned'},text])]];
for(const signature of ['legacy','',JSON.stringify({v:1,id:'message',phase:'final_answer'}),JSON.stringify({v:1,id:'message',phase:'bad'}),'{invalid',JSON.stringify({v:2,id:'wrong'}),'x'.repeat(65),'😀'.repeat(40),JSON.stringify({v:1,id:''})])histories.push([assistant([{...text,textSignature:signature},text])]);
for(const id of ['call+bad/|item+bad/','call|ctc_custom','a'.repeat(100)+'|z'.repeat(50),'a😀___|😀__','call|fc_item|ignored'])for(const extra of [{},{model:'other'},{provider:'foreign',api:'openai-responses'}])histories.push([assistant([{...call,id}],extra),{...result,toolCallId:id}]);
for(let i=0;i<histories.length;i++)for(const input of [['text'],['text','image']])add(`history-${i}-vision-${input.length}`,{...base,input},{...context,messages:histories[i]});
writeFileSync(process.argv[2],JSON.stringify({pin,models:models.map(model=>({model,levels:getSupportedThinkingLevels(model)})),cases},null,2)+'\n');
console.log(`Captured ${cases.length} Codex request cases and ${models.length} catalog entries.`);
}finally{unlinkSync(requestFile);unlinkSync(catalogFile);}
