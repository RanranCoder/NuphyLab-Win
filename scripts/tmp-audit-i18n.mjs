// Temporary audit script (not part of the build): check i18n key consistency.
import fs from "node:fs";
import path from "node:path";

const src = fs.readFileSync("shared/i18n.mjs", "utf8");
const m = src.match(/export const messages = \{([\s\S]*?)\n\};/);
if (!m) { console.log("PARSE FAIL"); process.exit(1); }
const messages = eval("({" + m[1] + "})");
console.log("messages count:", Object.keys(messages).length);

function walk(dir, acc = []) {
  for (const f of fs.readdirSync(dir)) {
    const p = path.join(dir, f);
    const st = fs.statSync(p);
    if (st.isDirectory()) walk(p, acc);
    else if (/\.(tsx?|mjs)$/.test(f)) acc.push(p);
  }
  return acc;
}
const files = [...walk("ui/src"), "shared/audio-effects.mjs", "shared/presets.mjs", "shared/claude-lighting.mjs", "shared/codex-lighting.mjs", "shared/trigger.mjs", "shared/games.mjs", "shared/color.mjs", "shared/audio-reactive.mjs", "shared/lighting-power.mjs", "shared/claude-keys.mjs", "shared/codex-keys.mjs"];
const missing = new Set();
const used = new Set();
for (const f of files) {
  const text = fs.readFileSync(f, "utf8");
  const re = /\b(?:tr|t)\(\s*(['"`])((?:\\.|(?!\1)[^\\])*)\1/gs;
  let mm;
  while ((mm = re.exec(text))) {
    const q = mm[1];
    let raw = mm[2];
    let val;
    if (q === "`") { if (raw.includes("${")) continue; val = raw; }
    else { try { val = JSON.parse('"' + raw + '"'); } catch { continue; } }
    if (!/[\u3400-\u9fff]/.test(val)) continue;
    used.add(val);
    if (!(val in messages)) missing.add(f + " :: " + val);
  }
}
console.log("used CJK strings:", used.size);
console.log("--- missing exact keys:", missing.size);
for (const x of [...missing]) console.log(x);

// Placeholder check: every {n} in English value must appear in Chinese key and vice versa
console.log("--- placeholder mismatches:");
const errors = [];
for (const patterns of [[/\{(\d+)\}/g, "num"], [/\{([a-zA-Z_]+)\}/g, "name"]]) {
  for (const [zh, en] of Object.entries(messages)) {
    const zhSet = [...zh.matchAll(patterns[0])].map(x => x[1]).sort().join(",");
    const enSet = [...en.matchAll(patterns[0])].map(x => x[1]).sort().join(",");
    if (zhSet !== enSet) errors.push(`[${patterns[1]}] key: ${zh} | zh:{${zhSet}} en:{${enSet}}`);
  }
}
for (const e of errors) console.log(e);
console.log("placeholder mismatch total:", errors.length);
