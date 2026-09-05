// Development-only oracle; executes the pinned implementation and Photon.
import {execFileSync} from 'node:child_process';
import {writeFileSync, mkdirSync} from 'node:fs';
import {createHash} from 'node:crypto';
import {pathToFileURL} from 'node:url';
const root=process.env.PI_REFERENCE!;
const pin='9841914c71a74d81abe07f751aefd271fd924e63';
if(execFileSync('git',['-C',root,'rev-parse','HEAD'],{encoding:'utf8'}).trim()!==pin)throw Error('Wrong Pi commit');
const load=(path:string)=>import(pathToFileURL(`${root}/${path}`).href);
const {processImage}=await load('packages/coding-agent/src/utils/image-process.ts');
const {detectSupportedImageMimeType}=await load('packages/coding-agent/src/utils/mime.ts');
const {loadPhoton}=await load('packages/coding-agent/src/utils/photon.ts');
const photon=await loadPhoton();if(!photon)throw Error('Photon must be available');
function pixels(w:number,h:number,noise=false){
 const bytes=new Uint8Array(w*h*4);let seed=7;
 for(let i=0;i<bytes.length;i+=4){seed=(Math.imul(seed,1664525)+1013904223)>>>0;bytes[i]=noise?seed&255:(i/4%w)<w/2?220:15;bytes[i+1]=noise?(seed>>>8)&255:90;bytes[i+2]=noise?(seed>>>16)&255:150;bytes[i+3]=255;}
 return new photon.PhotonImage(bytes,w,h);
}
const small=pixels(7,3),large=pixels(2400,1600),noise=pixels(64,48,true);
const png=Buffer.from(small.get_bytes()),jpeg=Buffer.from(small.get_bytes_jpeg(80));
const bmp=Buffer.alloc(70);bmp.write('BM');bmp.writeUInt32LE(70,2);bmp.writeUInt32LE(54,10);bmp.writeUInt32LE(40,14);bmp.writeInt32LE(2,18);bmp.writeInt32LE(2,22);bmp.writeUInt16LE(1,26);bmp.writeUInt16LE(24,28);bmp.fill(170,54);
const apng=Buffer.concat([png.subarray(0,33),Buffer.from('000000086163544c000000010000000000000000','hex'),png.subarray(33)]);
function oriented(value:number){const segment=Buffer.from('ffe1002245786966000049492a0008000000010012010300010000000100000000000000','hex');segment.writeUInt16LE(value,28);return Buffer.concat([jpeg.subarray(0,2),segment,jpeg.subarray(2)]);}
const cases:any[]=[];
async function add(name:string,bytes:Buffer,mime:string,options:any={}){
 const expected=await processImage(bytes,mime,options);
 if(expected.ok){const output=Buffer.from(expected.data,'base64');delete expected.data;expected.sha256=createHash('sha256').update(output).digest('hex');expected.byteLength=output.length;}
 cases.push({name,bytes:bytes.toString('base64'),mime,options,detected:detectSupportedImageMimeType(bytes.subarray(0,4100)),expected});
}
await add('png-original',png,'image/png');
await add('jpeg-original',jpeg,'image/jpeg');
await add('mime-alias',jpeg,' IMAGE/JPG; ignored');
await add('large-resize',Buffer.from(large.get_bytes()),'image/png');
await add('large-no-resize',Buffer.from(large.get_bytes()),'image/png',{autoResizeImages:false});
await add('bmp-conversion',bmp,'image/bmp');
await add('bmp-without-resize',bmp,'image/bmp',{autoResizeImages:false});
await add('gif-original',Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7','base64'),'image/gif');
await add('invalid-png-short',Buffer.from('89504e470d0a1a0a72657374','hex'),'image/png');
await add('invalid-png-header',png.subarray(0,24),'image/png');
await add('invalid-bmp',bmp.subarray(0,54),'image/bmp');
await add('animated-png-detection',apng,'image/png');
await add('jpeg-ls-detection',Buffer.from('ffd8fff700','hex'),'image/jpeg');
await add('noise-jpeg-fallback',Buffer.from(noise.get_bytes()),'image/png',{resizeOptions:{maxBytes:4000}});
await add('noise-reduce-dimensions',Buffer.from(noise.get_bytes()),'image/png',{resizeOptions:{maxBytes:900}});
await add('unachievable-limit',png,'image/png',{resizeOptions:{maxBytes:1}});
for(let i=1;i<=8;i++)await add(`exif-${i}`,oriented(i),'image/jpeg',{resizeOptions:{maxWidth:4,maxHeight:4}});
mkdirSync('tests/fixtures/images',{recursive:true});writeFileSync('tests/fixtures/images/small.png',png);
writeFileSync(process.argv[2],JSON.stringify({pin,cases},null,2)+'\n');
small.free();large.free();noise.free();
console.log(`Captured ${cases.length} image cases from pinned Pi, including output SHA-256 hashes.`);
