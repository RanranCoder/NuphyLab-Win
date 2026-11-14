import test from 'node:test';import assert from 'node:assert/strict';import {readFile} from 'node:fs/promises';import ts from '../ui/node_modules/typescript/lib/typescript.js';
// Windows checkouts keep CRLF in the working tree; the transpiler echoes the
// source's line endings, so normalize both sides to LF before comparing.
const lf=text=>text.replace(/\r\n/g,'\n');
test('server music validator is the complete UI validator compiled for the bundled runtime',async()=>{const source=lf(await readFile(new URL('../ui/src/music/model.ts',import.meta.url),'utf8'));const runtime=lf(await readFile(new URL('../shared/music-model.mjs',import.meta.url),'utf8'));assert.equal(runtime.split('\n').slice(1).join('\n'),lf(ts.transpileModule(source,{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ES2022}}).outputText));});
