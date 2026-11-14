import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdir,mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {verifyLayout,LAYOUT_SPEC,findIscc} from '../scripts/package-win.mjs';

async function fakeLayout(dir) {
  for (const entry of LAYOUT_SPEC) {
    const target = path.join(dir, ...entry.path.split('/'));
    if (entry.kind === 'file') {
      await mkdir(path.dirname(target), {recursive:true});
      await writeFile(target, 'placeholder');
    } else {
      await mkdir(target, {recursive:true});
      await writeFile(path.join(target, 'keep.txt'), 'x');
    }
  }
}
test('the packaged layout is verified entry by entry before anything ships',async t=>{
 const dir=await mkdtemp(path.join(tmpdir(),'nuphylab-layout-'));
 t.after(()=>rm(dir,{recursive:true,force:true}));
 await fakeLayout(dir);
 assert.deepEqual(await verifyLayout(dir),[]);
 await rm(path.join(dir,'bin','HIDBridge.exe'));
 await rm(path.join(dir,'server','keep.txt'));
 const problems=await verifyLayout(dir);
 assert.deepEqual(problems.sort(),['bin/HIDBridge.exe 缺失','server 是空目录']);
 await writeFile(path.join(dir,'bin','HIDBridge.exe'),'placeholder');
 await writeFile(path.join(dir,'server','keep.txt'),'x');
 await writeFile(path.join(dir,'runtime','node.exe'),'');
 await writeFile(path.join(dir,'NuPhyLab.exe'),'');
 const empties=(await verifyLayout(dir)).sort();
 assert.deepEqual(empties,['NuPhyLab.exe 是空文件','runtime/node.exe 是空文件']);
});
test('the layout spec covers the files the shell and server resolve at runtime',()=>{
 const paths=LAYOUT_SPEC.map(entry=>entry.path);
 for(const required of ['NuPhyLab.exe','WebView2Loader.dll','runtime/node.exe','runtime/LICENSE.node','bin/HIDBridge.exe','server','shared','hooks','ui/dist'])
  assert.ok(paths.includes(required),required);
});

test('ISCC resolution prefers the explicit path then the known install locations',async t=>{
 const dir=await mkdtemp(path.join(tmpdir(),'nuphylab-iscc-'));
 t.after(()=>rm(dir,{recursive:true,force:true}));
 const install=path.join(dir,'Inno Setup 6');
 await mkdir(install,{recursive:true});
 await writeFile(path.join(install,'ISCC.exe'),'');
 assert.equal(await findIscc({explicit:'C:\\x\\ISCC.exe'}),'C:\\x\\ISCC.exe');
 assert.equal(await findIscc({programFiles:dir}),path.join(install,'ISCC.exe'));
 assert.equal(await findIscc({programFiles:path.join(dir,'missing')}),null);
});
