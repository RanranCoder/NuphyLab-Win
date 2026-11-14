import test from 'node:test';
import assert from 'node:assert/strict';
import {APPEARANCES,DEFAULT_APPEARANCE,validateAppearance,effectiveTheme} from '../shared/appearance.mjs';
import {defaultSettings,validateSettings} from '../server/storage.mjs';

test('appearance defaults to light so a dark desktop does not repaint the studio',()=>{
 assert.equal(DEFAULT_APPEARANCE,'light');
 assert.equal(defaultSettings.appearance,'light');
 // A fresh library (no appearance key) must resolve to the light default.
 assert.equal(validateSettings({}).appearance,'light');
 assert.equal(validateSettings({appearance:'dark'}).appearance,'dark');
 assert.equal(validateSettings({appearance:'system'}).appearance,'system');
 // Omitting the key keeps whatever was stored.
 assert.equal(validateSettings({},validateSettings({appearance:'dark'})).appearance,'dark');
});

test('unknown appearance values are rejected instead of silently falling back',()=>{
 for(const bad of ['auto','dark ',1,null,[],{},true])
  assert.throws(()=>validateAppearance(bad),/外观设置无效/,JSON.stringify(bad));
 assert.throws(()=>validateSettings({appearance:'sepia'}),/外观设置无效/);
 assert.deepEqual([...APPEARANCES],['light','dark','system']);
});

test('the effective theme honours the explicit choice over the OS',()=>{
 assert.equal(effectiveTheme('light',true),'light','light wins over a dark desktop');
 assert.equal(effectiveTheme('dark',false),'dark','dark wins over a light desktop');
 assert.equal(effectiveTheme('system',true),'dark');
 assert.equal(effectiveTheme('system',false),'light');
 // An unknown or missing value behaves like the default.
 assert.equal(effectiveTheme(undefined,true),'light');
 assert.equal(effectiveTheme('bogus',true),'light');
});
