import test from 'node:test';
import assert from 'node:assert/strict';
import {selfContained,runtimePath,RUNTIME_VERSION,isDirectRun,runtimeArchive,peMachine,isWindowsExecutable,PE_MACHINE_X64,PE_MACHINE_ARM64} from '../scripts/fetch-node.mjs';
import {pathToFileURL} from 'node:url';

test('the runtime download command works from Unicode and spaced project paths without running on import',()=>{
 const entry='/tmp/项目/NuPhy Lab/scripts/fetch-node.mjs';
 assert.equal(isDirectRun(pathToFileURL(entry).href,entry),true);
 assert.equal(isDirectRun(pathToFileURL(entry).href,'/tmp/another-script.mjs'),false);
 assert.equal(isDirectRun(pathToFileURL(entry).href,''),false);
});

test('a runtime that only uses system libraries is accepted',()=>{
 assert.equal(selfContained(`/tmp/node:
\t/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation (compatibility version 150.0.0)
\t/usr/lib/libc++.1.dylib (compatibility version 1.0.0)
\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0)`),true);
});
test('a package-manager runtime that reaches outside the bundle is rejected',()=>{
 // Homebrew's node links against @rpath/libnode and /opt/homebrew libraries,
 // which do not exist on the machines this .app is handed to.
 assert.equal(selfContained(`/opt/homebrew/bin/node:
\t@rpath/libnode.147.dylib (compatibility version 0.0.0)
\t/usr/lib/libz.1.dylib (compatibility version 1.0.0)
\t/opt/homebrew/opt/libuv/lib/libuv.1.dylib (compatibility version 1.0.0)`),false);
 assert.equal(selfContained(`/x/node:\n\t/usr/local/lib/libfoo.dylib (compatibility version 1.0.0)`),false);
});
test('the bundled runtime is pinned and stored outside the source tree checkout',()=>{
 assert.match(RUNTIME_VERSION,/^v\d+\.\d+\.\d+$/);
 // path.join uses the host separator; keep the vendor layout assertion intact
 // on both POSIX and Windows.
 const sep=process.platform==='win32'?'\\\\':'/';
 const binary=process.platform==='win32'?'node\\.exe':'node';
 const vendor=(arch)=>new RegExp(`vendor${sep}node-${arch}${sep}${binary}$`);
 assert.match(runtimePath('arm64'),vendor('arm64'));
 assert.match(runtimePath('x64'),vendor('x64'));
});

test('the runtime archive matrix picks the platform entry the extractor expects',()=>{
 assert.deepEqual(runtimeArchive('v1.2.3','x64','win32'),
  {platform:'win-x64',archive:'zip',entry:'node-v1.2.3-win-x64/node.exe'});
 assert.deepEqual(runtimeArchive('v1.2.3','arm64','win32'),
  {platform:'win-arm64',archive:'zip',entry:'node-v1.2.3-win-arm64/node.exe'});
 assert.deepEqual(runtimeArchive('v1.2.3','arm64','darwin'),
  {platform:'darwin-arm64',archive:'tar.gz',entry:'node-v1.2.3-darwin-arm64/bin/node'});
});

test('the PE header check accepts the pinned x64 executable and rejects everything else',()=>{
 // Minimal valid x64 PE: MZ header, e_lfanew pointing at a PE signature.
 const pe=Buffer.alloc(0x100);
 pe.writeUInt16BE(0x4d5a,0); // MZ
 pe.writeUInt32LE(0x80,0x3c); // e_lfanew
 pe.writeUInt32LE(0x00004550,0x80); // PE\0\0
 pe.writeUInt16LE(PE_MACHINE_X64,0x84);
 assert.equal(peMachine(pe),PE_MACHINE_X64);
 assert.equal(isWindowsExecutable(pe,'x64'),true);
 pe.writeUInt16LE(PE_MACHINE_ARM64,0x84);
 assert.equal(isWindowsExecutable(pe,'x64'),false);
 assert.equal(isWindowsExecutable(pe,'arm64'),true);
 const truncated=pe.subarray(0,0x30);
 assert.equal(peMachine(truncated),null);
 assert.equal(peMachine(Buffer.from('not a pe file at all')),null);
 pe.writeUInt16BE(0x4d5b,0);
 assert.equal(peMachine(pe),null,'a broken MZ signature is rejected');
});

import {fakeService} from './helpers/fake-service.mjs';
test('the running app reports how it was signed so a stale grant can be explained',async t=>{
 // Absent signing.json (source runs) must not claim an ad-hoc build.
 const service=await fakeService(t);
 const build=(await service.state()).build;
 assert.equal(build.adhoc,false);
 assert.equal(typeof build.identity,'string');
});
