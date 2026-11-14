// Copies the Rust natives into their distribution slots, mirroring where the
// macOS build places the Swift binaries: HIDBridge.exe under bin/ for the
// Node server to spawn, and the NuPhyLab shell at the repo root (the packaged
// app runs it next to runtime/ and server/).
import { copyFile, mkdir } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const targetDir = path.join(root, "native-win", "target", "release");
const targets = [
  [path.join(targetDir, "hidbridge.exe"), path.join(root, "bin", "HIDBridge.exe")],
  [path.join(targetDir, "NuPhyLab.exe"), path.join(root, "NuPhyLab.exe")],
  // The GNU toolchain links WebView2Loader dynamically (MSVC links it
  // statically), so the DLL must sit next to NuPhyLab.exe.
  [path.join(root, "native-win", "shell", "dll", "WebView2Loader.dll"), path.join(root, "WebView2Loader.dll")],
];
for (const [, target] of targets) {
  await mkdir(path.dirname(target), { recursive: true });
}
let skipped = false;
for (const [source, target] of targets) {
  try {
    await copyFile(source, target);
    console.log(`${path.basename(source)} -> ${path.relative(root, target)}`);
  } catch (error) {
    if (error.code === "EBUSY" || error.code === "EPERM") {
      // A running dev session holds the executable locked; the file it is
      // running does not need replacing while that session is alive.
      console.warn(`skipped (in use): ${path.relative(root, target)} — close the running session and rebuild`);
      skipped = true;
    } else {
      console.error(`${path.basename(source)}: ${error.message}`);
      process.exit(1);
    }
  }
}
if (skipped) console.warn("Some artifacts were skipped; re-run after stopping the running session.");
