// Executes the actual pinned Pi builder and project context loader.
import {execFileSync} from 'node:child_process';
import {mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!;
const pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const load=(path:string)=>import(pathToFileURL(`${root}/${path}`).href);
const {buildSystemPrompt}=await load('packages/coding-agent/src/core/system-prompt.ts');
const tools=[];
for(const name of ['bash','read','edit','write','grep','find','ls']){
 const module=await load(`packages/coding-agent/src/core/tools/${name}.ts`);
 const tool=module[`create${name[0].toUpperCase()+name.slice(1)}ToolDefinition`]('/tmp');
 tools.push({name:tool.name,label:tool.label,description:tool.description,parameters:tool.parameters,promptSnippet:tool.promptSnippet,promptGuidelines:tool.promptGuidelines||[]});
}
const normalize=(value:string)=>value.replaceAll(`${root}/packages/coding-agent`,'/usr/share/doc/bashkitten/pi-reference');
const inputs=[
 {name:'default',cwd:'/project',contextFiles:[]},
 {name:'context',cwd:'/path with spaces/project',contextFiles:[{path:'/global/AGENTS.md',content:'Global instructions\n'},{path:'/project/AGENTS.override.md',content:'Local instructions\n\nKeep exact whitespace.'}]},
 {name:'custom',cwd:'C:\\project',customPrompt:'Custom system text.\n',appendSystemPrompt:'Append text.\n',contextFiles:[{path:'/project/AGENTS.md',content:'Project text.'}]},
 {name:'empty-custom',cwd:'/project',customPrompt:'',appendSystemPrompt:'Only appended text.',contextFiles:[]},
];
const cases=inputs.map(input=>({input,expected:normalize(buildSystemPrompt({...input,selectedTools:tools.map(t=>t.name),toolSnippets:Object.fromEntries(tools.map(t=>[t.name,t.promptSnippet])),promptGuidelines:tools.flatMap(t=>t.promptGuidelines)}))}));
// The full resource-loader eagerly imports an untracked external model catalog.
// Compile these exact, unmodified function bodies with their actual dependencies;
// no package/skill/catalog discovery code is needed to execute this oracle.
const loaderSource=readFileSync(`${root}/packages/coding-agent/src/core/resource-loader.ts`,'utf8');
const body=loaderSource.slice(loaderSource.indexOf('function loadContextFileFromDir('),loaderSource.indexOf('export interface DefaultResourceLoaderOptions'));
writeFileSync(`${root}/fixture-project-context.ts`,[
 "import { existsSync, readFileSync, statSync } from 'node:fs';",
 "import { basename, dirname, join, sep } from 'node:path';",
 "import chalk from 'chalk';",
 "import { canonicalizePath, resolvePath } from './packages/coding-agent/src/utils/paths.ts';",
 "import { stripBom } from './packages/coding-agent/src/utils/text.ts';",
 "import { findGitPaths } from './packages/coding-agent/src/core/footer-data-provider.ts';",
 body,
].join('\n'));
const {loadProjectContextFiles}=await load('fixture-project-context.ts');
const work=mkdtempSync(join(tmpdir(),'pi-prompt-oracle-'));
const files:any[]=[];
function scenario(name:string, entries:Record<string,string>,cwd:string,agentDir:string,setup?:()=>void){
 for(const [file,content] of Object.entries(entries)){const path=join(work,name,file);mkdirSync(join(path,'..'),{recursive:true});writeFileSync(path,content);}
 mkdirSync(join(work,name,cwd),{recursive:true});mkdirSync(join(work,name,agentDir),{recursive:true});setup?.();
 const output=loadProjectContextFiles({cwd:join(work,name,cwd),agentDir:join(work,name,agentDir)});
 files.push({name,files:entries,cwd,agentDir,expected:output.map((f:any)=>({...f,path:f.path.replace(join(work,name),'<ROOT>')}))});
}
scenario('precedence',{'global/AGENTS.md':'\uFEFFglobal','AGENTS.md':'root','repo/CLAUDE.md':'repo fallback','repo/nested/AGENTS.md':'nested ordinary','repo/nested/AGENTS.override.md':'override'},'repo/nested/child','global');
scenario('deduplicate-global',{'AGENTS.md':'root','repo/AGENTS.MD':'uppercase','repo/CLAUDE.md':'unused'},'repo/child','repo');
scenario('nested-worktree',{'repo/.git/HEAD':'ref: refs/heads/main','repo/.git/worktrees/feature/HEAD':'ref: refs/heads/feature','repo/.git/worktrees/feature/commondir':'../..','repo/AGENTS.md':'shadowed main','repo/nested/feature/.git':'gitdir: ../../.git/worktrees/feature','repo/nested/feature/AGENTS.md':'worktree copy'},'repo/nested/feature/src','global');
writeFileSync(process.argv[2],JSON.stringify({pin,tools,cases,contexts:files},null,2)+'\n');
rmSync(work,{recursive:true,force:true});
console.log(`Captured ${cases.length} prompts, ${files.length} project contexts and seven tool contracts from pinned Pi.`);
