// Temporary audit script (not part of the build): data consistency checks.
import fs from "node:fs";
import { layout, LED_COUNT, heart } from "../shared/layout.mjs";
import { createPresets } from "../shared/presets.mjs";
import triggerKeys from "../shared/trigger-keys.json" with { type: "json" };
import heartKeys from "../shared/heart-keys.json" with { type: "json" };

const hex = /^#[0-9a-f]{6}$/i;

// 1. layout integrity
console.log("layout keys:", layout.length, "LED_COUNT:", LED_COUNT);
const idx = layout.map(k => k.index);
console.log("index range:", Math.min(...idx), "-", Math.max(...idx), "unique:", new Set(idx).size === idx.length);
const missing = [...Array(LED_COUNT).keys()].filter(i => !idx.includes(i));
console.log("unmapped indices:", missing);
const codes = layout.map(k => k.code);
const dupCodes = codes.filter((c, i) => codes.indexOf(c) !== i);
console.log("duplicate layout codes:", dupCodes);

// 2. trigger-keys whitelist
console.log("trigger keys:", triggerKeys.length);
const tCodes = triggerKeys.map(k => k.code);
console.log("unique trigger codes:", new Set(tCodes).size === tCodes.length);
const tKeyCodes = triggerKeys.map(k => k.keyCode);
console.log("unique trigger keyCodes:", new Set(tKeyCodes).size === tKeyCodes.length, "min:", Math.min(...tKeyCodes), "max:", Math.max(...tKeyCodes));
const notInLayout = tCodes.filter(c => !codes.includes(c));
console.log("trigger codes with no LED in layout:", notInLayout);
const layoutOnly = codes.filter(c => !tCodes.includes(c));
console.log("layout codes not triggerable:", layoutOnly);

// 3. heart keys
const badHeart = heartKeys.filter(c => !codes.includes(c));
console.log("heart keys not in layout:", badHeart);
console.log("heart LEDs:", heart("#ffffff").filter(c => c !== "#000000").length, "expected", heartKeys.length);
// duplicate heart effect: two layout keys with same code (MetaRight?)
const metaRights = layout.filter(k => k.code === "MetaRight");
console.log("MetaRight keys in layout:", metaRights.map(k => k.index));
const metaLefts = layout.filter(k => k.code === "MetaLeft");
console.log("MetaLeft keys in layout:", metaLefts.map(k => k.index));

// 4. presets data
const presets = createPresets(layout);
console.log("presets:", presets.length);
let problems = 0;
for (const p of presets) {
  const checkColors = (colors, where) => {
    if (colors.length !== LED_COUNT) { console.log("BAD LEN", p.id, where, colors.length); problems++; }
    for (const c of colors) if (!hex.test(c)) { console.log("BAD COLOR", p.id, where, c); problems++; }
    if (colors[83] !== "#000000") { console.log("LED83 lit", p.id, where, colors[83]); problems++; }
  };
  checkColors(p.colors, "colors");
  if (p.animation.frames) for (const [i, f] of p.animation.frames.entries()) checkColors(f.colors, "frame" + i);
  if (p.animation.effect === "frames" && (!p.animation.frames || !p.animation.frames.length)) { console.log("empty frames", p.id); problems++; }
  const dur = (p.animation.frames ?? []).reduce((n, f) => n + f.duration, 0);
  if (p.animation.effect === "frames" && dur <= 0) { console.log("zero duration", p.id); problems++; }
}
console.log("preset problems:", problems);

// 5. frame speed handling sanity: renderAnimation at speed boundaries
const { renderAnimation, animationDuration } = await import("../shared/render-animation.mjs");
const colors = Array(84).fill("#ff0000");
for (const speed of [0, 1, 4, 10, 94, NaN]) {
  const anim = { effect: "frames", speed, loop: true, frames: [{ colors: Array(84).fill("#00ff00"), duration: 100 }, { colors: Array(84).fill("#0000ff"), duration: 100 }] };
  try {
    const a = renderAnimation(colors, anim, 0);
    const b = renderAnimation(colors, anim, 150);
    const d = animationDuration(anim);
    console.log("frames speed", speed, "t0:", a[0], "t150:", b[0], "duration:", d);
  } catch (e) { console.log("frames speed", speed, "THREW:", e.message); }
}
for (const speed of [0, 94, NaN]) {
  for (const effect of ["pulse", "wave", "move"]) {
    const anim = { effect, speed, loop: true, direction: "right", frames: [] };
    try {
      const a = renderAnimation(colors, anim, 12345);
      console.log(effect, "speed", speed, "ok, sample:", a[0], "dur:", animationDuration(anim));
    } catch (e) { console.log(effect, "speed", speed, "THREW:", e.message); }
  }
}

// 6. hsvToHex wrapHue edge
const { hsvToHex, hexToHsv } = await import("../shared/color.mjs");
for (const h of [0, 360, -360, 720, -1e-17, -1e-13, 359.9999999999999]) {
  try { console.log("hsvToHex h=" + h, hsvToHex({ h, s: 0.5, v: 1 })); }
  catch (e) { console.log("hsvToHex h=" + h, "THREW:", e.constructor.name, e.message); }
}
console.log("wrapHue(-1e-17):", JSON.stringify(-1e-17 % 360 < 0 ? (-1e-17 % 360) + 360 : -1e-17 % 360));
// roundtrip
let maxDiff = 0;
for (let r = 0; r <= 255; r += 5) for (let g = 0; g <= 255; g += 5) for (let b = 0; b <= 255; b += 5) {
  const hexs = "#" + [r, g, b].map(v => v.toString(16).padStart(2, "0")).join("");
  const { h, s, v } = hexToHsv(hexs);
  const back = hsvToHex({ h, s, v });
  if (back.toUpperCase() !== hexs.toUpperCase()) {
    const pr = parseInt(back.slice(1, 3), 16), pg = parseInt(back.slice(3, 5), 16), pb = parseInt(back.slice(5, 7), 16);
    maxDiff = Math.max(maxDiff, Math.abs(pr - r), Math.abs(pg - g), Math.abs(pb - b));
  }
}
console.log("hsv roundtrip max channel drift (grey/dark edges):", maxDiff);
