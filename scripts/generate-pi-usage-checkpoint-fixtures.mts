// Development-only pinned Pi oracle for numbered-file bookkeeping.
import {execFileSync} from 'node:child_process';
import {writeFileSync} from 'node:fs';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!, pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const load=(path:string)=>import(pathToFileURL(`${root}/${path}`).href);
const {FooterComponent}=await load('packages/coding-agent/src/modes/interactive/components/footer.ts');
const {setThemeInstance}=await load('packages/coding-agent/src/modes/interactive/theme/theme.ts');
const {createUsageTotals,addUsageToTotals}=await load('packages/coding-agent/src/core/usage-totals.ts');
setThemeInstance({fg:(_color:string,text:string)=>text,bold:(text:string)=>text});
const usage=(cost:number,input=8,cacheRead=24)=>({input,output:1,cacheRead,cacheWrite:0,totalTokens:input+cacheRead+1,cost:{input:cost,output:0,cacheRead:0,cacheWrite:0,total:cost}});
const assistant=(id:string,cost:number,input=8,cacheRead=24)=>({type:'message',id,parentId:null,timestamp:'2026-09-07T00:00:00.000Z',message:{role:'assistant',content:[{type:'text',text:'answer'}],api:'openai-completions',provider:'fixture',model:'model',stopReason:'stop',timestamp:0,usage:usage(cost,input,cacheRead)}});
const user={type:'message',id:'user',parentId:'before',timestamp:'2026-09-07T00:00:00.000Z',message:{role:'user',content:'next turn',timestamp:0}};
const compact={type:'compaction',id:'compact',parentId:'user',timestamp:'2026-09-07T00:00:00.000Z',summary:'summary',firstKeptEntryId:'user',tokensBefore:1000,usage:usage(0.0001,50,0)};
const cases=[];
for(const [name,omitted,retained] of [
 ['cache-without-retained-assistant',[assistant('before',0.1)],[user,compact]],
 ['zero-usage-clears-cache',[assistant('old',0.1),assistant('before',0.2,0,0)],[user,compact]],
 ['cost-addition-order',[assistant('before',1e16)],[user,assistant('small-a',1),assistant('small-b',1),compact]],
] as any[]) {
 const history=[...omitted,...retained];
 const totals=createUsageTotals();for(const entry of history){if(entry.message?.usage)addUsageToTotals(totals,entry.message.usage);else if(entry.usage)addUsageToTotals(totals,entry.usage);}
 const context={tokens:null,contextWindow:2000,percent:null};
 const view=new FooterComponent({state:{model:{provider:'fixture',id:'model',contextWindow:2000},thinkingLevel:'off'},sessionManager:{getEntries:()=>history,getCwd:()=>'/fixture',getSessionName:()=>undefined},getContextUsage:()=>context,modelRuntime:{isUsingSubscription:()=>false}},{getGitBranch:()=>undefined,getAvailableProviderCount:()=>1,getExtensionStatuses:()=>new Map()});
 view.setAutoCompactEnabled(true);
 cases.push({name,omitted,retained,totals,text:view.render(1000)[1].split(/ {2,}/)[0]});
}
writeFileSync(process.argv[2],JSON.stringify({pin,cases},null,2)+'\n');
