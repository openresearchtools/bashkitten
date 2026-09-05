// Development-only oracle. Never shipped or invoked by the Rust package.
// Run with pinned Pi's tsx and tsconfig.fixture.json (see tests/fixtures/README.md).
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
const root = process.env.PI_REFERENCE!;
const pin = '9841914c71a74d81abe07f751aefd271fd924e63';
if (execFileSync('git', ['-C', root, 'rev-parse', 'HEAD'], {encoding:'utf8'}).trim() !== pin) throw Error('Wrong Pi commit');
const load = (path: string) => import(pathToFileURL(`${root}/${path}`).href);
const pi = await load('packages/coding-agent/src/core/compaction/compaction.ts');
const session = await load('packages/coding-agent/src/core/session-manager.ts');
const retry = await load('packages/ai/src/utils/retry.ts');
const overflow = await load('packages/ai/src/utils/overflow.ts');
const usage = {input:100,output:10,cacheRead:20,cacheWrite:0,totalTokens:130,cost:{input:0.1,output:0.2,cacheRead:0.01,cacheWrite:0,total:0.31}};
const assistant = (content: any[], extra = {}) => ({role:'assistant',api:'openai-completions',provider:'fixture',model:'model',content,usage,stopReason:'stop',timestamp:1000,...extra});
const text = (value: string) => ({type:'text',text:value});
const user = (value: string) => ({role:'user',content:[text(value)],timestamp:1});
const entries = (messages: any[]) => messages.map((message, i) => ({type:'message',id:`e${i}`,parentId:i?`e${i-1}`:null,timestamp:new Date(1000+i).toISOString(),message}));
const settings = {enabled:true,reserveTokens:200,keepRecentTokens:20};
const histories: any[] = [
    ['empty',[]],
    ['tiny',entries([user('hi'),assistant([text('hello')])])],
    ['history',entries([user('old '.repeat(300)),assistant([text('done')]),user('new '.repeat(30)),assistant([text('latest')])])],
    ['split',entries([user('old '.repeat(300)),assistant([text('answer '.repeat(30))]),user('new'),assistant([text('latest')])])],
    ['tools',entries([user('old '.repeat(300)),assistant([{type:'toolCall',id:'call',name:'edit',arguments:{path:'a.ts',edits:[{oldText:'a',newText:'b'}]}}]),{role:'toolResult',toolCallId:'call',toolName:'edit',content:[text('done')],isError:false,timestamp:1001},user('new '.repeat(30)),assistant([text('latest')])])],
    ['unicode',entries([user('🙂漢字 '.repeat(300)),assistant([text('done')]),user('new '.repeat(30)),assistant([{type:'thinking',thinking:'thought',thinkingSignature:'signature'},text('answer')])])],
];
const previous = entries([user('old '.repeat(300)),assistant([text('done')]),user('kept '.repeat(20)),assistant([text('done')])]);
previous.push({type:'compaction',id:'c0',parentId:'e3',timestamp:new Date(1100).toISOString(),summary:'previous checkpoint',firstKeptEntryId:'e2',tokensBefore:1000,details:{readFiles:['a.ts'],modifiedFiles:['b.ts']},usage} as any);
previous.push({...entries([user('recent '.repeat(30)),assistant([text('end')])])[0],id:'e4',parentId:'c0'});
previous.push({...entries([user('unused'),assistant([text('end')])])[1],id:'e5',parentId:'e4'});
histories.push(['iterative', previous]);
const model = {api:'openai-completions',id:'model',provider:'fixture',name:'Fixture',baseUrl:'http://127.0.0.1',reasoning:true,input:['text','image'],contextWindow:2000,maxTokens:1000,cost:{input:0,output:0,cacheRead:0,cacheWrite:0}};
const cases=[];
for (const [name, history] of histories) {
    const messages = session.buildSessionContext(history).messages;
    const preparation = pi.prepareCompaction(history, settings);
    const requests: any[]=[];
    let result;
    if (preparation) {
        result=await pi.compact(preparation, model, undefined, undefined, 'preserve tests', undefined, 'low', (_model:any,context:any,options:any) => {
            requests.push({systemPrompt:context.systemPrompt,messages:context.messages.map(({timestamp,...message}:any)=>message),options:{maxTokens:options.maxTokens,reasoning:options.reasoning,cacheRetention:options.cacheRetention,sessionId:options.sessionId}});
            return {result:async()=>assistant([text(`summary ${requests.length}`)], {usage})};
        }, undefined, undefined, undefined, 'fixture-session');
    }
    const clean = preparation ? {...preparation,fileOps:{read:[...preparation.fileOps.read],written:[...preparation.fileOps.written],edited:[...preparation.fileOps.edited]}} : null;
    cases.push({name,entries:history,settings,messages,estimate:pi.estimateContextTokens(messages),cut:pi.findCutPoint(history,0,history.length,20),preparation:clean,requests,result:result??null});
}
const errors=['overloaded','429','insufficient_quota','billing','maximum context length is 2000 tokens','too many requests','request_too_large','stream ended before a terminal response event','Request aborted','ResourceExhausted','400 (no body)','rate limit context length exceeded','exceeds the available context size'];
const recovery=errors.map(error=>{const message=assistant([], {stopReason:'error',errorMessage:error,usage:{...usage,input:0,output:0,cacheRead:0,totalTokens:0}});return {message,retryable:retry.isRetryableAssistantError(message),overflow:overflow.isContextOverflow(message,2000)};});
const footer = await load('packages/coding-agent/src/modes/interactive/components/footer.ts');
const theme = await load('packages/coding-agent/src/modes/interactive/theme/theme.ts');
// Suppress terminal ANSI styling only; render() still performs Pi's calculations.
theme.setThemeInstance({fg: (_color: string, text: string) => text, bold: (text: string) => text});
const usageCases=[];
for (const item of cases) {
    for (const subscription of [false,true]) {
        const history = item.entries;
        const contextWindow = 2000;
        const boundary = history.findLastIndex((entry:any)=>entry.type==='compaction');
        const fresh = history.slice(boundary+1).some((entry:any)=>entry.message?.role==='assistant' && !['error','aborted'].includes(entry.message.stopReason) && pi.calculateContextTokens(entry.message.usage)>0);
        const context = boundary>=0 && !fresh ? {tokens:null,contextWindow,percent:null} : {tokens:item.estimate.tokens,contextWindow,percent:item.estimate.tokens/contextWindow*100};
        const state = {model:{...model,provider:subscription?'openai-codex':'fixture'},thinkingLevel:'off'};
        const view = new footer.FooterComponent({state,sessionManager:{getEntries:()=>history,getCwd:()=>'/fixture',getSessionName:()=>undefined},getContextUsage:()=>context,modelRuntime:{isUsingSubscription:()=>subscription}}, {getGitBranch:()=>undefined,getAvailableProviderCount:()=>1,getExtensionStatuses:()=>new Map()});
        const auto = !subscription;
        view.setAutoCompactEnabled(auto);
        const text=view.render(1000)[1].split(/ {2,}/)[0];
        usageCases.push({name:item.name,entries:history,contextWindow,subscription,auto,text});
    }
}
const fixedCases=[0,0.0625,0.0005,0.3125,1.25,2.55,9.95,99.95,100.005].flatMap(value=>[1,3].map(digits=>({value,digits,text:value.toFixed(digits)})));
const tokenCases=[0,999,1000,1050,1250,2550,9999,10000,10500,999499,999500,1000000,9999999,10000000].map(value=>({value,text:footer.formatTokens(value)}));
writeFileSync(process.argv[2],JSON.stringify({pin,model,summaryUsage:usage,cases,recovery,usageCases,fixedCases,tokenCases},null,2)+'\n');
console.log(`Generated ${cases.length} compaction cases, ${recovery.length} recovery cases and ${usageCases.length} footer cases from Pi ${pin}`);
