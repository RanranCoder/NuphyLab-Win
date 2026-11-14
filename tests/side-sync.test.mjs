import test from 'node:test';
import assert from 'node:assert/strict';
import { fakeService, until } from './helpers/fake-service.mjs';

const wait = (ms) => new Promise((r) => setTimeout(r, ms));
const art = Array(84).fill('#123456');

test('connecting agent lighting with nothing to show leaves the side strip untouched', async t => {
  const s = await fakeService(t);
  await s.command({ op: 'settings', settings: { codexEnabled: true, codexLightMode: 'whole', brightness: 100 } });
  await until(s.state, v => v.playback.mode === 'agent', 'agent lighting did not start');
  await wait(700);
  const state = await s.state();
  assert.equal(state.device.state[10], 65, 'connecting must not darken the strip');
  assert.equal(
    (await s.operations()).filter(x => x.op === 'sideSync' || x.op === 'side').length,
    0,
    'no side writes may happen while the agent frame is fully dark',
  );
  await s.configure({ tasks: [{ id: 'a', status: 'running' }] });
  await until(
    async () => (await s.operations()).filter(x => x.op === 'sideSync').at(-1),
    op => op && op.color === '#304ffe',
    'a running task never reached the side strip',
    8000,
  );
});

test('agent lighting follows the side strip to the task colour instead of muting it', async t => {
  const s = await fakeService(t, { tasks: [{ id: 'a', status: 'running' }] });
  await s.command({ op: 'settings', settings: { codexEnabled: true, codexSelectedPulse: false, brightness: 100 } });
  const synced = await until(
    async () => (await s.operations()).filter(x => x.op === 'sideSync').at(-1),
    op => op && op.color === '#304ffe',
    'agent status colour never reached the side strip',
    8000,
  );
  assert.ok(synced.brightness > 0, 'the synced strip stays lit');
  assert.equal(
    (await s.operations()).filter(x => x.op === 'side' && x.color === '#000000' && x.brightness === 0).length,
    0,
    'connecting agent lighting must not mute the side strip',
  );
  await s.command({ op: 'stop' });
  const stopped = await s.state();
  assert.equal(stopped.device.state[10], 65, 'stopping restores the original side brightness');
  assert.equal(stopped.device.recoveryPending, false);
});

test('static artwork syncs the side strip and repeats without extra writes', async t => {
  const s = await fakeService(t);
  await s.command({ op: 'settings', settings: { autoConnect: false, codexEnabled: false, brightness: 100 } });
  await s.command({ op: 'preview', colors: art });
  await until(
    async () => (await s.operations()).filter(x => x.op === 'sideSync').at(-1),
    op => op && op.color === '#123456',
    'static artwork never reached the side strip',
  );
  const after = (await s.operations()).filter(x => x.op === 'sideSync').length;
  await wait(300);
  const before = (await s.operations()).filter(x => x.op === 'sideSync').length;
  assert.ok(before <= after, 'an unchanged frame keeps the side strip unchanged');
  await s.command({ op: 'stop' });
  assert.equal((await s.state()).device.state[10], 65, 'stop restores the original side configuration');
});

test('a quota colour keeps owning the strip while it is enabled', async t => {
  const s = await fakeService(t);
  await s.command({ op: 'settings', settings: { quotaSideEnabled: true } });
  await until(s.state, state => state.device.sideColor === '#6f9e85', 'quota colour was not applied');
  await s.command({ op: 'preview', colors: art });
  await until(s.state, state => state.playback.mode === 'static', 'preview did not start');
  await wait(400);
  const operations = await s.operations();
  assert.equal(
    operations.filter(x => x.op === 'sideSync').length,
    0,
    'sync stays idle while quota owns the strip',
  );
  await s.command({ op: 'stop' });
});
