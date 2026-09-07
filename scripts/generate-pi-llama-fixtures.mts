// Development-only, offline oracle. Algorithm source is unchanged; appended
// exports expose private pure helpers. No router or Hugging Face traffic occurs.
import {execFileSync} from 'node:child_process';
import {readFileSync,writeFileSync,mkdtempSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!;
const pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const temp=mkdtempSync(`${tmpdir()}/pi-llama-fixture-`);
const source=`${root}/packages/coding-agent/src/extensions/llama`;
writeFileSync(`${temp}/client.ts`,readFileSync(`${source}/client.ts`,'utf8')+'\nexport {parseLoadProgress,parseDownloadProgress};\n');
const client=await import(pathToFileURL(`${temp}/client.ts`).href);
const {HuggingFaceClient}=await import(pathToFileURL(`${source}/huggingface.ts`).href);
const cases:any[]=[];
const record=async(name:string,kind:string,input:any,run:()=>any)=>{try{cases.push({name,kind,input,expected:(await run())??null});}catch(e){cases.push({name,kind,input,error:e.message});}};
for(const input of [' http://localhost:8080/v1///?x=1#abc ','https://example.org/prefix/v1','https://example.org/v1/v1/','ftp://localhost','bad','http://LOCALHOST:80/a/../v1'])await record(`url-${cases.length}`,'url',input,()=>client.normalizeLlamaServerUrl(input));
for(const input of [0,1023,1024,10240,1048575,1048576,2**40,2**50,-1,1234.567])await record(`bytes-${input}`,'bytes',input,()=>client.formatBytes(input));
for(const input of [null,{}, {progress:{}},{progress:{current:'load_tensors',value:.2,stages:['load_model','load_tensors']}},{progress:{stage:'x',value:7,stages:[1,'x']}},{progress:{stage:'unknown',value:-2}},{progress:{current:'',stage:'other',value:.5}}])await record(`load-progress-${cases.length}`,'loadProgress',input,()=>client.parseLoadProgress(input));
for(const input of [null,{}, {x:{done:1024,total:4096},y:{done:512,total:1024}},{progress:{x:{done:99,total:50}}},{progress:{x:{done:1,total:0}}},{a:{done:'1',total:20}}])await record(`download-progress-${cases.length}`,'downloadProgress',input,()=>client.parseDownloadProgress(input));
let response:any={};let requests:any[]=[];
globalThis.fetch=async(url:any,init:any={})=>{requests.push({url:String(url),method:init.method||'GET',authorization:new Headers(init.headers).get('authorization'),body:init.body?JSON.parse(init.body):null});return new Response(response.raw??JSON.stringify(response.payload),{status:response.status??200,headers:response.headers});};
for(const payload of [{data:[]},{data:[{id:'a',status:{value:'loaded'},extra:7}]},[],{},null,{data:[{id:'a'}]},{data:[{id:4,status:{value:'loaded'}}]}]){
 response={payload};requests=[];await record(`list-${cases.length}`,'list',{payload},()=>new client.LlamaClient('http://router:8080/v1','fixture-key').list({reload:true}));cases.at(-1).requests=requests;
}
for(const payload of [null,{models_autoload:true},{models_autoload:'true'},{models_autoload:false},[]]){response={payload};requests=[];await record(`props-${cases.length}`,'props',{payload},()=>new client.LlamaClient('http://router:8080','fixture-key').props());cases.at(-1).requests=requests;}
for(const method of ['load','unload','download']){response={payload:{}};requests=[];await record(method,method,{},()=>new client.LlamaClient('http://router:8080','fixture-key')[method]('test/model:Q4_K_M'));cases.at(-1).requests=requests;}
const siblings=[{rfilename:'model.Q4_K_M-00001-of-00002.gguf',size:300},{rfilename:'model.Q4_K_M-00002-of-00002.gguf',size:350},{rfilename:'model.IQ2_XXS.gguf',size:90},{rfilename:'model.Q8_0.gguf'},{rfilename:'model.F16.gguf',size:1500},{rfilename:'mmproj-Q4_K_M.gguf',size:20},{rfilename:'folder/model.UD-IQ1_S.gguf',size:80},{rfilename:'README.md',size:9},{rfilename:'unknown.gguf',size:30}];
for(const payload of [null,{},[],{id:'org/name',gated:'manual',siblings},{gated:'auto',siblings:[{rfilename:'a.MXFP4.gguf',size:20}]},{gated:true,siblings:[{rfilename:'A.q4_k_m.GGUF',size:8},{rfilename:'B.Q4_K_M.gguf'}]}]){response={payload};requests=[];await record(`hf-details-${cases.length}`,'hfDetails',{payload},()=>new HuggingFaceClient('fixture-token','http://hf/').details('a b/model!'));cases.at(-1).requests=requests;}
for(const payload of [[{id:'a',downloads:3},{id:'b',downloads:'3'},null,{},7],{},null]){response={payload};requests=[];await record(`hf-search-${cases.length}`,'hfSearch',{payload},()=>new HuggingFaceClient('fixture-token','http://hf').search('tiny model/gguf'));cases.at(-1).requests=requests;}
for(const input of [{status:429,headers:{'retry-after':'15'}},{status:429,headers:{'retry-after':'0',ratelimit:'r=0;t=37'}},{status:429,headers:{}},{status:401,payload:{error:'Gated repository'}},{status:503,raw:'not json'}]){response=input;requests=[];await record(`hf-error-${cases.length}`,'hfError',input,()=>new HuggingFaceClient().search('fixture'));}
for(const value of ['NaN','inf','Infinity','-Infinity','0x10','0B101','0o17','+0x10','  .5  ','1e21','1e-8','1e309','-0','false','1junk']){const input={status:429,headers:{'retry-after':value,ratelimit:'r=0;t=37'}};response=input;requests=[];await record(`hf-retry-number-${value}`,'hfError',input,()=>new HuggingFaceClient().search('fixture'));}
for(const input of [{status:500,payload:{error:{message:'Router failed'}}},{status:503,raw:'bad json'},{status:404,payload:{error:'ignored'}}]){response=input;requests=[];await record(`router-error-${cases.length}`,'routerError',input,()=>new client.LlamaClient('http://router').list());}
writeFileSync(process.argv[2],JSON.stringify({pin,cases},null,2)+'\n');rmSync(temp,{recursive:true});console.log(`Captured ${cases.length} pinned Pi router/HF cases without network traffic.`);
