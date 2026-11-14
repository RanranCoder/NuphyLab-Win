// Generates the Windows application icon from the supplied logo. Replaces the
// macOS icon.swift/iconutil chain; png2icons is pure JavaScript, so this runs
// on every platform the package can be built on.
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const png2icons = createRequire(import.meta.url)("png2icons");
const source = path.join(root, "logo", "头像.png");
const target = path.join(root, "assets", "AppIcon.ico");

const input = await readFile(source);
const ico = png2icons.createICO(input, png2icons.BICUBIC, 0, false);
if (!ico) throw new Error(`无法从 ${source} 生成 ICO，请检查 logo 文件格式。`);
await mkdir(path.dirname(target), { recursive: true });
await writeFile(target, ico);
console.log(`AppIcon.ico generated: ${path.relative(root, target)} (${ico.length} bytes)`);
