import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {ClaudeActionRunner,claudeAppInstalled,claudeAppPath,describeClaudeAction} from '../server/claude-actions.mjs';

const IS_WIN=process.platform==='win32';
// A fake child process good enough for the runner: execRaw needs a stdin
// stream, exercised spawns are never awaited.
const fakeChild=()=>({stdin:{end(){}},unref(){}});

test('claudeAppPath finds the first existing candidate',async t=>{
 const dir=await mkdtemp(path.join(tmpdir(),'nuphylab-claude-actions-'));
 t.after(()=>rm(dir,{recursive:true,force:true}));
 const first=path.join(dir,'mac','Claude.app'),second=path.join(dir,'claude.exe');
 await writeFile(second,'');
 assert.equal(await claudeAppPath([first,second]),second);
 assert.equal(await claudeAppInstalled([first,second]),true);
 assert.equal(await claudeAppPath([first]),null);
 assert.equal(await claudeAppInstalled([first]),false);
});

test('copyResume text uses the platform shell form and quotes spaces',async()=>{
 const cwd=IS_WIN?'D:\\项目 目录':'/tmp/项目 目录';
 const action=describeClaudeAction('copyResume',{session:{id:'local_abc',cwd}});
 assert.equal(action.available,true);
 if(IS_WIN) assert.equal(action.text,'cd /d "D:\\项目 目录" && claude --resume local_abc');
 else assert.equal(action.text,`cd '${cwd}' && claude --resume local_abc`);
 // The runner hands exactly that text to the clipboard helper.
 let pasted=null;
 const runner=new ClaudeActionRunner({execRaw:(file,options,callback)=>{pasted={file,options};setImmediate(()=>callback(null));return fakeChild();}});
 assert.deepEqual(await runner.execute(action),{copied:true,text:action.text});
 if(IS_WIN) assert.ok(pasted.file.endsWith('clip.exe'),'Windows uses clip.exe');
 else assert.equal(pasted.file,'/usr/bin/pbcopy');
});

test('openSession, focusApp and revealProject use platform-native carriers',async t=>{
 const launched=[],execd=[];
 const runner=new ClaudeActionRunner({
  exec:async(file,args)=>{execd.push([file,...args]);},
  launch:(file,args,options)=>{launched.push([file,...args,options]);return fakeChild();},
 });
 if(IS_WIN){
  const session=await runner.execute({available:true,type:'openSession',url:'claude://code/continue?session=local_ab'});
  assert.equal(session.opened,'claude://code/continue?session=local_ab');
  const start=execd.at(-1);
  assert.equal(start[0],path.join(process.env.SystemRoot??'C:\\Windows','System32','cmd.exe'));
  assert.equal(start[1],'/d');
  assert.equal(start.at(-1),'start "" "claude://code/continue?session=local_ab"');
  const focused=await runner.execute({available:true,type:'focusApp',appPath:'C:\\Claude\\claude.exe'});
  assert.equal(focused.activated,'C:\\Claude\\claude.exe');
  assert.equal(launched.at(-1)[0],'C:\\Claude\\claude.exe');
  assert.equal(launched.at(-1).length,2,'focusApp launches the exe with no arguments');
  const dir=await mkdtemp(path.join(tmpdir(),'nuphylab-reveal-'));
  t.after(()=>rm(dir,{recursive:true,force:true}));
  const opened=await runner.execute({available:true,type:'revealProject',path:dir});
  assert.equal(opened.opened,dir);
  assert.deepEqual(launched.at(-1).slice(0,2),['explorer.exe',dir]);
  await assert.rejects(()=>runner.execute({available:true,type:'revealProject',path:path.join(dir,'missing')}),/可能已被移动或删除/);
 }else{
  await runner.execute({available:true,type:'openSession',url:'claude://code/continue?session=local_ab'});
  assert.deepEqual(execd.at(-1),['/usr/bin/open','--','claude://code/continue?session=local_ab']);
  const focused=await runner.execute({available:true,type:'focusApp',bundleId:'com.anthropic.claudefordesktop'});
  assert.equal(focused.activated,'com.anthropic.claudefordesktop');
  await runner.execute({available:true,type:'revealProject',path:'/tmp'});
  assert.deepEqual(execd.at(-1),['/usr/bin/open','--','/tmp']);
 }
});

test('focusApp fails honestly when no Claude executable exists',{skip:await claudeAppInstalled()?'a real Claude install is present on this machine':false},async()=>{
 const runner=new ClaudeActionRunner({launch:()=>fakeChild()});
 await assert.rejects(()=>runner.execute({available:true,type:'focusApp'}),/无法切换到 Claude 应用/);
});
