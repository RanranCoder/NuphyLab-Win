import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdir,mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {findSignTool,selectCertificate,signArguments,verifyArguments,signAll,signingHandle,CERT_ENUM_COMMAND} from '../scripts/signing-win.mjs';

const hashA='A'.repeat(40), hashB='B'.repeat(40);
const single=`  ${hashA}  NuPhyLab Code Signing\n`;
const listing=`  ${hashA}  NuPhyLab Code Signing\n  ${hashB}  Another Certificate\n`;

test('signtool is found explicitly, on PATH, or newest-first in the Windows Kits tree',async t=>{
 const dir=await mkdtemp(path.join(tmpdir(),'nuphylab-signtool-'));
 t.after(()=>rm(dir,{recursive:true,force:true}));
 // Newest SDK first, x64 preferred; the older version has no x64 file so the
 // x86 fallback of the newest must still win over the older version.
 await mkdir(path.join(dir,'10.0.22621.0','x64'),{recursive:true});
 await writeFile(path.join(dir,'10.0.22621.0','x64','signtool.exe'),'');
 await mkdir(path.join(dir,'10.0.19041.0','x86'),{recursive:true});
 await writeFile(path.join(dir,'10.0.19041.0','x86','signtool.exe'),'');
 assert.equal(await findSignTool({explicit:'C:\\tools\\signtool.exe',where:async()=>{throw new Error('not on path');}}),'C:\\tools\\signtool.exe');
 assert.equal(await findSignTool({where:async()=>({stdout:'C:\\path\\signtool.exe\n'})}),path.normalize('C:\\path\\signtool.exe'));
 const found=await findSignTool({kitsRoot:dir,where:async()=>{throw new Error('not on path');}});
 assert.match(found,/10\.0\.22621\.0\\x64\\signtool\.exe$/);
 assert.equal(await findSignTool({kitsRoot:path.join(dir,'missing'),where:async()=>{throw new Error('no');}}),null);
});

test('certificate selection demands exactly one code-signing identity unless one is named',()=>{
 assert.deepEqual(selectCertificate(single),{hash:hashA,subject:'NuPhyLab Code Signing'});
 assert.deepEqual(selectCertificate(listing,hashB),{hash:hashB,subject:'Another Certificate'});
 assert.deepEqual(selectCertificate(listing,'Another Certificate'),{hash:hashB,subject:'Another Certificate'});
 assert.throws(()=>selectCertificate(listing,hashA+'0'),/不存在或不唯一/);
 assert.throws(()=>selectCertificate(listing),/AIR_STUDIO_SIGN_IDENTITY/);
 assert.throws(()=>selectCertificate(''),/没有可用于代码签名的证书/);
});

test('signing arguments separate the certificate source from the timestamp policy',()=>{
 const files=['a.exe','b.exe'];
 const byPfx=signArguments('tool.exe',{pfx:'cert.pfx',pfxPassword:'secret'},files);
 assert.deepEqual(byPfx,[
  ['tool.exe',['sign','/fd','SHA256','/f','cert.pfx','/p','secret','/tr','http://timestamp.digicert.com','/td','SHA256','a.exe']],
  ['tool.exe',['sign','/fd','SHA256','/f','cert.pfx','/p','secret','/tr','http://timestamp.digicert.com','/td','SHA256','b.exe']],
 ]);
 const byHash=signArguments('tool.exe',{hash:hashA},files,{timestamp:false});
 for(const [,args] of byHash) assert.deepEqual(args,['sign','/fd','SHA256','/sha1',hashA,args.at(-1)]);
 assert.deepEqual(verifyArguments('tool.exe',files),[
  ['tool.exe',['verify','/pa','/all','a.exe']],
  ['tool.exe',['verify','/pa','/all','b.exe']],
 ]);
});

test('signAll signs then verifies each file and never runs for an unsigned handle',async()=>{
 const calls=[];
 const run=async(tool,args)=>{calls.push([path.basename(tool),args[0]]);return {stdout:''};};
 const files=['a.exe','b.exe'];
 await signAll({tool:'tool.exe',hash:hashA},files,{run});
 assert.deepEqual(calls.map(([tool,verb])=>`${tool}:${verb}`),
  ['tool.exe:sign','tool.exe:sign','tool.exe:verify','tool.exe:verify']);
 calls.length=0;
 await signAll({unsigned:true,name:'unsigned'},files,{run});
 assert.deepEqual(calls,[]);
});

test('the signing handle degrades only through the explicit unsigned opt-in',async()=>{
 // No signtool anywhere: an unsigned package needs the explicit opt-out.
 await assert.rejects(()=>signingHandle({signTool:async()=>null}),/signtool/);
 const unsigned=await signingHandle({signTool:async()=>null,allowUnsigned:true});
 assert.equal(unsigned.unsigned,true);
 // A PFX short-circuits the store enumeration entirely.
 let enumerated=false;
 const pfx=await signingHandle({signTool:async()=>'tool.exe',pfx:'my.pfx',certificates:async()=>{enumerated=true;}});
 assert.equal(enumerated,false);
 assert.equal(pfx.unsigned,false);
 assert.match(pfx.name,/^PFX:my\.pfx$/);
 const fromStore=await signingHandle({signTool:async()=>'tool.exe',certificates:async()=>single});
 assert.equal(fromStore.hash,hashA);
 assert.equal(fromStore.unsigned,false);
 // The enumeration command only ever reads the certificate stores.
 assert.match(CERT_ENUM_COMMAND.at(-1),/Get-ChildItem Cert:\\/);
 assert.doesNotMatch(CERT_ENUM_COMMAND.at(-1),/Remove-Item|Export|Set-Item/);
});
