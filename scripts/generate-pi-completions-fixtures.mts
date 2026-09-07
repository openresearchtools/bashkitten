// Offline development oracle; actual pinned algorithms, appended exports only.
import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,unlinkSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!;const pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
globalThis.fetch=async()=>{throw Error('No network in completions fixtures');};
const file=`${root}/packages/ai/src/api/.fixture-openai-completions.ts`;
writeFileSync(file,readFileSync(`${root}/packages/ai/src/api/openai-completions.ts`,'utf8')+'\nexport {getCompat,buildParams};\n');
try {
const {getCompat,buildParams}=await import(pathToFileURL(file).href);
const {buildBaseOptions}=await import(pathToFileURL(`${root}/packages/ai/src/api/simple-options.ts`).href);
const base={id:'fixture-model',name:'Fixture',api:'openai-completions',provider:'fixture',baseUrl:'http://localhost:8080/v1',contextWindow:128000,maxTokens:16384,reasoning:true,input:['text','image'],cost:{input:0,output:0,cacheRead:0,cacheWrite:0}};
const tool={name:'read',description:'Read a file',parameters:{type:'object',properties:{path:{type:'string'}},required:['path']}};
const context={systemPrompt:'System instructions',messages:[{role:'user',content:[{type:'text',text:'Question'}],timestamp:1}],tools:[tool]};
const cases:any[]=[];
const add=(name:string,model:any={},ctx:any=context,options:any={},simple=false)=>{
 model={...base,...model};const resolved=simple?{...buildBaseOptions(model,ctx,options),reasoningEffort:options.reasoningEffort,thinkingBudgets:options.thinkingBudgets}:options;
 try {cases.push({name,model,context:ctx,options,simple,compat:getCompat(model),expected:buildParams(model,ctx,resolved)});}catch(e){cases.push({name,model,context:ctx,options,simple,error:e.message});}
};
add('default');add('non-reasoning',{reasoning:false});add('text-only',{input:['text']});
for(const [provider,baseUrl,id] of [['openai','https://api.openai.com/v1','gpt-fixture'],['fixture','https://api.z.ai/api/paas/v4','glm'],['fixture','https://api.deepseek.com/v1','deepseek-reasoner'],['fixture','https://api.together.ai/v1','qwen'],['fixture','https://api.moonshot.cn/v1','kimi'],['fixture','https://integrate.api.nvidia.com/v1','deepseek'],['openrouter','https://openrouter.ai/api/v1','anthropic/claude'],['openrouter','https://openrouter.ai/api/v1','openai/gpt'],['fixture','https://api.x.ai/v1','grok'],['fixture','https://gateway.ai.cloudflare.com/v1','model'],['fixture','https://api.ant-ling.com/v1','model'],['fixture','https://cerebras.ai/v1','model']])add(`detect-${provider}-${id}`,{provider,baseUrl,id},{...context},{sessionId:'fixture-session',reasoningEffort:'medium'});
for(const format of ['openai','openrouter','deepseek','together','baseten','zai','qwen','qwen-chat-template','chat-template','string-thinking','ant-ling'])for(const effort of [undefined,'medium'])for(const mapping of [undefined,{off:null,medium:null},{off:'none',medium:'custom'}])add(`thinking-${format}-${effort||'off'}-${JSON.stringify(mapping)}`,{thinkingLevelMap:mapping,compat:{thinkingFormat:format,supportsReasoningEffort:true,thinkingTokenBudgetField:'thinking_budget_tokens',chatTemplateKwargs:{enable:{$var:'thinking.enabled'},effort:{$var:'thinking.effort'},budget:{$var:'thinking.budget'},preserve:true,conditional:{$var:'thinking.effort',omitWhenOff:true}},chatTemplateArgs:{enabled:{$var:'thinking.enabled'},effort:{$var:'thinking.effort'}}}},context,{reasoningEffort:effort,maxTokens:4000});
for(const max of [100,1024,1025,4096,16384])add(`budget-${max}`,{compat:{thinkingFormat:'chat-template',thinkingTokenBudgetField:'thinking_token_budget',chatTemplateKwargs:{budget:{$var:'thinking.budget'}}}},context,{maxTokens:max,reasoningEffort:'high',thinkingBudgets:{high:22000}});
const assistant=(content:any,extra:any={})=>({role:'assistant',content,api:'openai-completions',provider:'fixture',model:'fixture-model',stopReason:'stop',timestamp:1,...extra});
const call={type:'toolCall',id:'call',name:'read',arguments:{path:'a'}};
const result={role:'toolResult',toolCallId:'call',toolName:'read',content:[{type:'text',text:'output'}],isError:false,timestamp:2};
const text={type:'text',text:'answer'};const thinking={type:'thinking',thinking:'reasoning',thinkingSignature:'reasoning_content'};
add('unicode-system',{}, {...context,systemPrompt:'\ud83d'});
add('unicode-tool-only',{}, {...context,messages:[assistant([call]),{...result,content:[{type:'text',text:'\ud83d'}]}]});
add('unicode-thinking-as-text',{compat:{requiresThinkingAsText:true}}, {...context,messages:[assistant([{...thinking,thinking:'\ud83d'}])]});
add('unicode-thinking-raw',{}, {...context,messages:[assistant([{...thinking,thinking:'\ud83d'},text])]});
const histories:any[]=[
 [assistant([text])],[assistant([{type:'text',text:' \n '},text])],[assistant([])],[assistant([thinking])],[assistant([thinking,text])],[assistant([{...thinking,thinkingSignature:'reasoning_text'},text])],[assistant([{...thinking,thinkingSignature:undefined},text])],
 [assistant([call]),result,{role:'user',content:'next',timestamp:3}],
 [assistant([call]),{...result,content:[{type:'image',mimeType:'image/png',data:'aGVsbG8='}]}],
 [assistant([call]),{role:'user',content:'interrupted',timestamp:3}],
 [assistant([call])],
 [assistant([thinking,text,call],{provider:'foreign',api:'openai-responses',model:'other'}),result],
 [assistant([thinking,text],{stopReason:'error'})],
 [{role:'user',content:[{type:'text',text:'before'},{type:'image',mimeType:'image/png',data:'xx'},{type:'image',mimeType:'image/png',data:'yy'},{type:'text',text:'after'}],timestamp:1}],
 [assistant([{...call,id:'same|different/item+'},{...call,id:'same|second/item+'}],{provider:'foreign',api:'openai-responses'}),{...result,toolCallId:'same|different/item+'},{...result,toolCallId:'same|second/item+'}],
];
for(let index=0;index<histories.length;index++)for(const compat of [{},{requiresAssistantAfterToolResult:true,requiresToolResultName:true,requiresThinkingAsText:true},{requiresReasoningContentOnAssistantMessages:true}])add(`history-${index}-${JSON.stringify(compat)}`,{compat},{...context,messages:histories[index]});
for(const input of [['text'],['text','image']])add(`images-${input.length}`,{input},{...context,messages:histories[13]});
for(const cacheRetention of ['none','short','long'])add(`cache-${cacheRetention}`,{provider:'openrouter',id:'anthropic/claude',baseUrl:'https://openrouter.ai/api/v1'},{...context,messages:[assistant([text])]},{cacheRetention,sessionId:'x'.repeat(90)});
add('compat-overrides',{compat:{supportsStore:false,supportsDeveloperRole:false,supportsUsageInStreaming:false,supportsStrictMode:false,maxTokensField:'max_tokens',zaiToolStream:true,vllmPriority:-3,openRouterRouting:{only:['one']},vercelGatewayRouting:{order:['two']}}},context,{reasoningEffort:'high',maxTokens:1000,temperature:.3,toolChoice:'required'});
add('sampling-overrides',{},context,{samplingParams:{model:'override',stream:false,store:true,max_tokens:999,top_p:.4}});
add('simple-context-clamp',{contextWindow:8192,maxTokens:8192},context,{reasoningEffort:'medium'},true);
writeFileSync(process.argv[2],JSON.stringify({pin,cases},null,2)+'\n');console.log(`Captured ${cases.length} pinned completions request cases without network.`);
}finally{unlinkSync(file);}
