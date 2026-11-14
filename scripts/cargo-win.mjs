// Runs cargo with a working GNU binutils directory prepended to PATH.
//
// Rust's windows-gnu toolchain links raw-dylib imports (windows-sys and
// friends) by invoking `dlltool`, and the dlltool bundled under the
// toolchain's self-contained directory cannot spawn its assembler. A real
// mingw-w64 binutils (dlltool + as) must come first on PATH. GitHub Actions
// windows runners ship one; local machines may need MSYS2 or mingw-w64.
//
// Extra candidate directories can be supplied via the MINGW_BINS env var
// (OS path-list separator separated), probed before the defaults.
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { delimiter, join } from "node:path";

const args = process.argv.slice(2);
if (!args.length) {
  console.error("usage: node scripts/cargo-win.mjs <cargo args...>");
  process.exit(2);
}

function probe(text, args2) {
  try {
    return spawnSync(text, args2, { encoding: "utf8" }).stdout?.trim() ?? "";
  } catch {
    return "";
  }
}

const sysroot = probe("rustc", ["--print", "sysroot"]);
const host = (probe("rustc", ["-vV"]).match(/host: (\S+)/) || [])[1];
const candidates = [
  ...(process.env.MINGW_BINS ? process.env.MINGW_BINS.split(delimiter) : []),
  "C:\\msys64\\mingw64\\bin",
  "C:\\dev\\msys64\\mingw64\\bin",
  "C:\\mingw64\\bin",
  host && sysroot ? join(sysroot, "lib", "rustlib", host, "bin", "self-contained") : "",
].filter(Boolean);
const binutils = candidates.find((dir) => existsSync(join(dir, "dlltool.exe")) && existsSync(join(dir, "as.exe")));

const env = { ...process.env };
// The spread loses process.env's case-insensitive key behavior: on machines
// where the variable is stored as `Path`, assigning `env.PATH` creates a
// duplicate key and the spawned cargo fails to resolve (ENOENT). Reuse the
// casing that is actually present.
const pathKey = Object.keys(env).find((key) => key.toLowerCase() === "path") ?? "Path";
if (binutils) {
  // Order matters: rustc resolves plain `dlltool` from PATH, so the working
  // binutils must shadow the toolchain's broken self-contained copy.
  env[pathKey] = `${binutils}${delimiter}${env[pathKey] || ""}`;
  console.log(`cargo-win: using binutils from ${binutils}`);
} else {
  console.log("cargo-win: no extra binutils found (relying on PATH, as CI does)");
}

const result = spawnSync("cargo", args, { stdio: "inherit", env });
process.exit(result.status ?? 1);
