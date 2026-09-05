// Execute all seven real pinned tools and their argument preparation/validation.
import {execFileSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync,symlinkSync,existsSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!,pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const load=(path:string)=>import(pathToFileURL(`${root}/${path}`).href);
const {validateToolArguments}=await load('packages/ai/src/utils/validation.ts');
const definitions:any={};
for(const name of ['bash','read','edit','write','grep','find','ls']){
 const module=await load(`packages/coding-agent/src/core/tools/${name}.ts`);
 definitions[name]=module[`create${name[0].toUpperCase()+name.slice(1)}ToolDefinition`];
}
const workspace=mkdtempSync(join(tmpdir(),'pi-tools-'));
const cases:any[]=[];
async function add(name:string,tool:string,args:any,files:Record<string,string>={},extra:any={}){
 if(process.env.PI_TOOL_CASE_PREFIX&&!name.startsWith(process.env.PI_TOOL_CASE_PREFIX))return;
 if(extra.rawArgs)args=JSON.parse(extra.rawArgs);
 const cwd=join(workspace,name);mkdirSync(cwd);
 for(const [name,contents] of Object.entries(files)){const file=join(cwd,name);mkdirSync(join(file,'..'),{recursive:true});writeFileSync(file,contents);}
 for(const [name,contents] of Object.entries(extra.binaryFiles??{}))writeFileSync(join(cwd,name),Buffer.from(contents as string,'base64'));
 for(const file of extra.rawFiles??[])writeFileSync(Buffer.concat([Buffer.from(cwd+'/'),Buffer.from(file.nameBase64,'base64')]),Buffer.from(file.contentBase64,'base64'));
 for(const dir of extra.dirs??[])mkdirSync(join(cwd,dir),{recursive:true});
 for(const [name,target] of Object.entries(extra.links??{}))symlinkSync(target as string,join(cwd,name));
 const definition=definitions[tool](cwd),call={type:'toolCall',id:'fixture',name:tool,arguments:structuredClone(args)};
 let expected:any; const updates:any[]=[];
 try {
  if(definition.prepareArguments)call.arguments=definition.prepareArguments(call.arguments);
  const validated=validateToolArguments(definition,call);
  if(extra.validationOnly)expected={validated};
  else{const controller=new AbortController();if(extra.aborted)controller.abort();const result=await definition.execute(call.id,validated,controller.signal,(result:any)=>{if(extra.captureUpdates)updates.push(structuredClone(result));if(extra.abortWhenOutput&&result.content.some((c:any)=>c.text?.includes(extra.abortWhenOutput)))controller.abort();},{cwd,sessionManager:{getSessionId:()=>undefined,getSessionFile:()=>undefined},model:{input:extra.nonVision?['text']:['text','image']}});expected={result};}
 } catch(error:any){expected={error:error.message};}
 if(extra.captureUpdates)expected.updates=updates;
 const changed:any={};for(const name of extra.inspect??[])try{changed[name]=readFileSync(join(cwd,name),'utf8');}catch{changed[name]=null;}
 const directories:any={};for(const name of extra.inspectDirectories??[])directories[name]=existsSync(join(cwd,name));
 expected=JSON.parse(JSON.stringify(expected).replaceAll(cwd,'<ROOT>'));
 cases.push({name,tool,args,files,...extra,expected,changed,directories});
}
const text={'text.txt':'one\ntwo\nthree\n'};
for(const [name,args] of Object.entries({normal:{path:'text.txt'},offset:{path:'text.txt',offset:2,limit:1},zero:{path:'text.txt',offset:0},negative:{path:'text.txt',offset:-2},fraction:{path:'text.txt',offset:1.5,limit:1.5},'negative-limit':{path:'text.txt',limit:-1},'negative-end':{path:'text.txt',offset:2,limit:-3},'zero-limit':{path:'text.txt',limit:0},'beyond-end':{path:'text.txt',offset:8},coerced:{path:'text.txt',offset:'2',limit:true},'optional-null':{path:'text.txt',offset:null,limit:null},'at-path':{path:'@text.txt'},'normalization':{path:'gone/../text.txt'}}))await add('read-'+name,'read',args,text);
await add('read-missing','read',{path:'missing'});
await add('read-directory','read',{path:'.'});
await add('read-empty','read',{path:'empty'},{empty:''});
await add('read-unicode-nfd','read',{path:'caf\u00e9.txt'},{'cafe\u0301.txt':'decomposed'});
await add('read-screenshot','read',{path:'shot 2 PM.png'},{'shot 2\u202fPM.png':'screenshot path'});
await add('read-curly','read',{path:"capture d'ecran.txt"},{'capture d\u2019ecran.txt':'curly quote'});
await add('read-too-long','read',{path:'text.txt'},{'text.txt':'x'.repeat(52000)+'\nlast'});
for(const tool of Object.keys(definitions)) {
 const args:any={read:{path:'text.txt'},write:{path:'out','content':'hi'},edit:{path:'text.txt',edits:[{oldText:'one',newText:'new'}]},bash:{command:'printf hi'},grep:{pattern:'one'},find:{pattern:'*.txt'},ls:{}};
 await add(tool+'-pre-aborted',tool,args[tool],text,{aborted:true,inspect:['out','text.txt']});
 await add(tool+'-bad-required',tool,{...args[tool],[tool==='bash'?'command':tool==='grep'||tool==='find'?'pattern':'path']:[]},text,{validationOnly:true});
 await add(tool+'-missing-required',tool,{},text,{validationOnly:true});
}
await add('validate-multiple-errors','read',{path:[],offset:'bad',limit:{}},{},{validationOnly:true});
await add('validate-nested','edit',{path:22,edits:[{oldText:3,newText:false},{oldText:[],newText:{}}]},{},{validationOnly:true});
await add('validate-null-required','write',{path:null,content:45},{},{validationOnly:true});
await add('validate-legacy-edit','edit',{path:'file',oldText:'a',newText:'b'},{},{validationOnly:true});
for(const limit of [0,-1,1.5,2])await add('ls-limit-'+limit,'ls',{limit},{'a.txt':'a','B.txt':'b','c.txt':'c'});
await add('ls-symlinks','ls',{},text,{links:{broken:'missing',linked:'text.txt'},dirs:['dir','.hidden']});
await add('ls-order','ls',{},{'a_':'','a-':'','ab':'','a.b':'','.hidden':'','a b':'','\u00e9':'','z':'','\u00e4':''});
await add('write-coerce','write',{path:7,content:42},{},{inspect:['7']});
await add('write-unicode','write',{path:'nested/out',content:'hello \ud83d\ude42\n'},{},{inspect:['nested/out']});
await add('write-directory','write',{path:'.',content:'oops'});
await add('edit-normal','edit',{path:'text.txt',edits:[{oldText:'two',newText:'changed'}]},text,{inspect:['text.txt']});
await add('edit-missing','edit',{path:'missing',edits:[{oldText:'a',newText:'b'}]});
await add('edit-empty','edit',{path:'text.txt',edits:[]},text);
await add('edit-overlap','edit',{path:'text.txt',edits:[{oldText:'one\ntwo',newText:'x'},{oldText:'two',newText:'y'}]},text,{inspect:['text.txt']});
for(const [name,before,edits] of [
 ['distant',Array.from({length:30},(_,i)=>`line ${i+1}`).join('\n')+'\n',[{oldText:'line 3\n',newText:'third\n'},{oldText:'line 26\n',newText:'twenty six\n'}]],
 ['nearby','one\ntwo\nthree\nfour\nfive\n',[{oldText:'two',newText:'second'},{oldText:'four',newText:'fourth'}]],
 ['insert','one\ntwo\nthree\n',[{oldText:'two',newText:'before\ntwo\nafter'}]],
 ['delete','one\ntwo\nthree\n',[{oldText:'two\n',newText:''}]],
 ['no-newline','one\ntwo',[{oldText:'two',newText:'last'}]],
 ['add-newline','one\ntwo',[{oldText:'two',newText:'two\n'}]],
 ['remove-newline','one\ntwo\n',[{oldText:'two\n',newText:'two'}]],
 ['repeated','x\na\nx\nb\nx\nc\nx\n',[{oldText:'a\nx\nb',newText:'b\nx\na'}]],
] as const)await add('edit-'+name,'edit',{path:'file',edits},{file:before},{inspect:['file']});
await add('grep-normal','grep',{pattern:'one'},text);
await add('grep-context','grep',{pattern:'two',context:1},text);
await add('grep-empty','grep',{pattern:'none'},text);
await add('grep-missing','grep',{pattern:'one',path:'missing'},text);
await add('find-normal','find',{pattern:'*.txt'},text);
await add('find-missing','find',{pattern:'*.txt',path:'missing'},text);
await add('bash-output','bash',{command:'printf out; printf err >&2'});
await add('bash-error','bash',{command:'printf failed; exit 7'});
await add('bash-empty','bash',{command:'true'});
for(const limit of [0,-1,1.5])await add('grep-limit-'+limit,'grep',{pattern:'o',limit},text);
await add('grep-fraction-context','grep',{pattern:'two',context:0.5},text);
await add('grep-cr','grep',{pattern:'two',context:1},{'text.txt':'one\rtail\ntwo\r\nthree\n'});
await add('grep-emoji','grep',{pattern:'marker'},{'text.txt':'marker'+ '\ud83d\ude42'.repeat(300)});
for(const args of [{offset:'0x2'},{offset:'TRUE'},{offset:' '},{offset:false}])await add('read-coerce-'+cases.length,'read',{path:'text.txt',...args},text);
await add('validate-bool','grep',{pattern:true,ignoreCase:'TRUE',literal:'0',context:null},{},{validationOnly:true});
await add('validate-root','read',[],{},{validationOnly:true});
await add('validate-required-order','write',{path:[]},{},{validationOnly:true});
await add('bash-zero-timeout','bash',{command:'true',timeout:0});
await add('bash-negative-timeout','bash',{command:'true',timeout:-1});
await add('bash-timeout','bash',{command:'sleep 30',timeout:0.02});
await add('bash-abort-output','bash',{command:'printf ready; sleep 30'},{},{abortWhenOutput:'ready'});
// Inside a repository both fd 8 (Bookworm) and fd 10 accept Pi's arguments.
// Their own argument-parser diagnostics differ; capture both without rewriting.
for(const limit of [-1,0,0.5,1.5,1e21])await add('find-boundary-'+limit,'find',{pattern:'*.txt',limit},text,{dirs:['.git']});
for(const [name,value] of Object.entries({small:1e-7,large:1e21,negativeZero:-0,rounded:1000000000000000100}))
 await add('validate-number-string-'+name,'write',{path:'out',content:value},{},{validationOnly:true});
for(const [name,value] of Object.entries({hexLarge:'0x10000000000000000',hexRounded:'0x100000000000000080',binaryLarge:'0b1'+'0'.repeat(65),octalLarge:'0o1'+'0'.repeat(23),bom:'\ufeff2\ufeff',nextLine:'\u00852\u0085',inf:'inf',plusInf:'+inf',infinity:'Infinity',badHex:'0x',signedHex:'+0x2',spaceTrue:' true ',exponent:'1e21',small:'1e-7'}))
 await add('validate-number-input-'+name,'read',{path:'text.txt',offset:value},{},{validationOnly:true});
for(const offset of [1.5,2.5])await add('read-fraction-long-'+offset,'read',{path:'text.txt',offset},{'text.txt':'x'.repeat(52000)+'\n'+'y'.repeat(52000)});
for(const limit of [-0.5,-1.5,0.5,1e-7])await add('read-fraction-limit-'+limit,'read',{path:'text.txt',limit},text);
await add('read-number-error-format','read',{path:'text.txt',offset:1e21},text);
for(const [name,before,oldText] of [
 ['circled','Keep ① here\nuntouched ②\n','Keep 1 here'],
 ['roman','Value Ⅷ\n','Value VIII'],
 ['accent','Cafe\u0300\n','Cafè'],
 ['hangul','\u1100\u1161 test\n','가 test'],
 ['superscript','Area m²\n','Area m2'],
 ['ligature','ﬅuff\n','stuff'],
 ['bom-trailing','marker\ufeff\n','marker\n'],
 ['nextline-trailing','marker\u0085\n','marker\n'],
 ['math-letter','𝐀 text\n','A text'],
] as const)await add('edit-normalize-'+name,'edit',{path:'file',edits:[{oldText,newText:'changed'}]},{file:before},{inspect:['file']});
for(const [name,value] of Object.entries({tieEven:'0x20000000000001',tieOdd:'0x20000000000003',roundSticky:'0x2000000000000101',wideRound:'0x1fffffffffffffff80',leadingZero:'0x'+'0'.repeat(25)+'123',overflow:'0x1'+'0'.repeat(256),maximum:'0x'+'f'.repeat(255)+'8000000000000',invalidDigit:'0b102',separator:'0x1_2'}))
 await add('validate-radix-round-'+name,'read',{path:'text.txt',offset:value},{},{validationOnly:true});
await add('validate-radix-maximum-finite','read',{path:'text.txt',offset:'0x'+'f'.repeat(13)+'8'+'0'.repeat(242)},{},{validationOnly:true});
await add('validate-diagnostic-numbers','read',{path:[],offset:1e21,limit:1e-7,extra:{'3':'third','1':'first',negativeZero:-0,nested:[1000000000000000100,1.0,null]}},{},{validationOnly:true});
await add('validate-diagnostic-empty','read',{path:{},offset:[],extra:[{},[],{},[]]},{},{validationOnly:true});
for(const [name,rawArgs] of Object.entries({float:'{"path":[],"offset":1.0,"limit":-0.0}',rounded:'{"path":[],"extra":{"3":9007199254740993,"1":1000000000000000128}}',number:'{"path":"text.txt","offset":9007199254740993}'}))
 await add('validate-raw-number-'+name,'read',{}, {},{validationOnly:true,rawArgs});
const invalidText=Buffer.from('before\nmarker\xffafter\nend\n','latin1').toString('base64');
for(const context of [0,1,0.5])await add('grep-invalid-utf8-'+context,'grep',{pattern:'marker',context},{},{binaryFiles:{'file.txt':invalidText}});
await add('read-invalid-utf8','read',{path:'file.txt'},{},{binaryFiles:{'file.txt':invalidText}});
for(const limit of [1,2])await add('grep-invalid-filename-'+limit,'grep',{pattern:'marker',limit},{},{rawFiles:[{nameBase64:Buffer.from('bad\xff.txt','latin1').toString('base64'),contentBase64:Buffer.from('marker\n').toString('base64')}]});
for(const tool of ['read','write','edit','grep','find','ls'])await add(tool+'-nul-path',tool,{path:'bad\0path',content:'hi',edits:[{oldText:'a',newText:'b'}],pattern:'*'},{},{dirs:['.git']});
await add('write-nul-parent','write',{path:'bad\0path/child',content:'hi'});
await add('write-nul-new-parent','write',{path:'nested/bad\0path',content:'hi'},{},{inspectDirectories:['nested']});
await add('edit-directory','edit',{path:'.',edits:[{oldText:'a',newText:'b'}]});
for(const [name,path] of Object.entries({single:"a'b\0",both:"a'b\"c\0",ticks:"a'b\"c`\0",interp:"a'b\"${c}\0",control:'a\u000b\u001a\u0085\0'}))await add('read-nul-'+name,'read',{path});
for(const tool of ['write','edit'])for(const aborted of [false,true]){
 const args={path:'file',content:'new',edits:[{oldText:'a',newText:'b'}]};
 await add(tool+'-link-cycle-'+aborted,tool,args,{},{links:{file:'file'},aborted});
 await add(tool+'-nul-abort-'+aborted,tool,{...args,path:'bad\0path'},{},{aborted});
}
await add('grep-empty-glob','grep',{pattern:'one',glob:''},text);
await add('grep-nul-pattern','grep',{pattern:'a\0b'},text);
await add('grep-nul-glob','grep',{pattern:'one',glob:'a\0b'},text);
await add('find-nul-pattern','find',{pattern:'a\0b'},text,{dirs:['.git']});
await add('bash-nul-command','bash',{command:'printf a\0b'});
for(const [name,command] of Object.entries({empty:'true',short:'printf hello',burst:'printf A; sleep 0.03; printf B; sleep 0.3; printf C',descendant:'(printf A; sleep 0.04; printf B; sleep 0.04; printf C) & exit 0'}))
 await add('bash-updates-'+name,'bash',{command},{},{captureUpdates:true});
await add('bash-updates-aborted','bash',{command:'true'},{},{captureUpdates:true,aborted:true});
await add('bash-updates-invalid','bash',{command:'true',timeout:0},{},{captureUpdates:true});
await add('bash-post-exit-abort','bash',{command:'(printf ready; sleep 0.03; printf again; sleep 5) & exit 0'},{},{captureUpdates:true,abortWhenOutput:'readyagain'});
await add('bash-post-exit-timeout','bash',{command:'(printf ready; sleep 5) & exit 0',timeout:0.03},{},{captureUpdates:true});
const {getToolPath}=await load('packages/coding-agent/src/utils/tools-manager.ts');
const fdVersion=execFileSync(getToolPath('fd'),['--version'],{encoding:'utf8'}).trim();
writeFileSync(process.argv[2],JSON.stringify({pin,fdVersion,cases},null,2)+'\n');
rmSync(workspace,{recursive:true,force:true});console.log(`Captured ${cases.length} pinned tool cases.`);
