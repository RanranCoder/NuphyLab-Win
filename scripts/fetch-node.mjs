// Downloads the pinned, self-contained Node runtime that gets bundled into the
// .app / the Windows package. Package managers such as Homebrew ship a
// dynamically linked node that depends on files outside the bundle, which would
// not run on another machine.
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile, copyFile, chmod } from "node:fs/promises";
import { promisify } from "node:util";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const exec = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
export const RUNTIME_VERSION = process.env.AIR_STUDIO_NODE_VERSION ?? "v24.21.0";
const IS_WIN = process.platform === "win32";
const runtimeBinary = () => (IS_WIN ? "node.exe" : "node");
export const runtimePath = (arch = process.arch) =>
  path.join(root, "vendor", `node-${arch}`, runtimeBinary());

export function runtimeArchive(version, arch, platform = process.platform) {
  if (platform === "win32") return { platform: `win-${arch}`, archive: "zip", entry: `node-${version}-win-${arch}/node.exe` };
  return { platform: `darwin-${arch}`, archive: "tar.gz", entry: `node-${version}-darwin-${arch}/bin/node` };
}

async function download(url) {
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`下载失败 ${response.status}：${url}`);
  return Buffer.from(await response.arrayBuffer());
}
export async function fetchRuntime({ version = RUNTIME_VERSION, arch = process.arch } = {}) {
  const { platform, archive, entry } = runtimeArchive(version, arch);
  const name = `node-${version}-${platform}`;
  const base = `https://nodejs.org/dist/${version}`;
  const sums = (await download(`${base}/SHASUMS256.txt`)).toString("utf8");
  const expected = sums.split("\n").map((line) => line.trim().split(/\s+/))
    .find(([, file]) => file === `${name}.${archive}`)?.[0];
  if (!expected) throw new Error(`官方校验文件中没有 ${name}.${archive}`);
  const archiveBuffer = await download(`${base}/${name}.${archive}`);
  const actual = createHash("sha256").update(archiveBuffer).digest("hex");
  if (actual !== expected) throw new Error("Node 运行时校验失败，已放弃使用下载的文件。");
  const work = await mkdtemp(path.join(tmpdir(), "nuphylab-node-"));
  try {
    const file = path.join(work, `node.${archive}`);
    await writeFile(file, archiveBuffer);
    await extractRuntime(file, work, name, archive);
    const target = runtimePath(arch);
    await mkdir(path.dirname(target), { recursive: true });
    await copyFile(path.join(work, entry), target);
    if (!IS_WIN) await chmod(target, 0o755);
    await verifyRuntime(target);
    return { target, version, sha256: actual };
  } finally {
    await rm(work, { recursive: true, force: true });
  }
}
async function extractRuntime(file, work, name, archive) {
  if (archive === "tar.gz") {
    await exec("/usr/bin/tar", ["xzf", file, "-C", work, `${name}/bin/node`]);
    return;
  }
  // Windows 10 ships bsdtar, which handles zip and supports selective extract.
  try {
    await exec(path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe"),
      ["-xf", file, "-C", work, name]);
  } catch {
    await exec("powershell.exe", [
      "-NoProfile", "-NonInteractive", "-Command",
      `Expand-Archive -LiteralPath '${file}' -DestinationPath '${work}' -Force`,
    ]);
  }
}

/** Parses a PE header: machine type and presence of a valid MZ/PE signature. */
export function peMachine(buffer) {
  if (buffer.length < 0x40 || buffer.readUInt16BE(0) !== 0x4d5a) return null; // "MZ"
  const peOffset = buffer.readUInt32LE(0x3c);
  if (peOffset <= 0 || peOffset + 6 > buffer.length) return null;
  if (buffer.readUInt32LE(peOffset) !== 0x00004550) return null; // "PE\0\0"
  return buffer.readUInt16LE(peOffset + 4);
}
export const PE_MACHINE_X64 = 0x8664;
export const PE_MACHINE_ARM64 = 0xaa64;
/** Rejects a runtime that is not the official standalone Windows executable. */
export function isWindowsExecutable(buffer, arch = "x64") {
  const machine = peMachine(buffer);
  return machine === (arch === "arm64" ? PE_MACHINE_ARM64 : PE_MACHINE_X64);
}
/** Rejects a runtime that depends on files outside the macOS bundle. */
export function selfContained(output) {
  return output.split("\n").slice(1).map((line) => line.trim().split(/\s+/)[0]).filter(Boolean)
    .every((library) => library.startsWith("/usr/lib/") || library.startsWith("/System/"));
}
export async function verifyRuntime(binary) {
  if (IS_WIN) {
    // Windows node.exe from the official zip needs no dylib audit; the PE
    // header check rejects truncated files and wrong-architecture binaries.
    const buffer = await readFile(binary);
    if (!isWindowsExecutable(buffer, process.arch === "arm64" ? "arm64" : "x64"))
      throw new Error(
        `Node 运行时 ${binary} 不是有效的 Windows x64 可执行文件，打包后在其他电脑上无法启动。\n` +
          "请运行 npm run fetch-node 下载官方运行时，或用 AIR_STUDIO_NODE 指定一个。",
      );
    return true;
  }
  const { stdout } = await exec("/usr/bin/otool", ["-L", binary]);
  if (!selfContained(stdout))
    throw new Error(
      `Node 运行时 ${binary} 依赖 .app 之外的动态库，打包后在其他 Mac 上无法启动。\n` +
        "请运行 npm run fetch-node 下载官方自包含运行时，或用 AIR_STUDIO_NODE 指定一个。",
    );
  return true;
}
export function isDirectRun(moduleURL, entry = process.argv[1]) {
  return Boolean(entry) && fileURLToPath(moduleURL) === path.resolve(entry);
}
if (isDirectRun(import.meta.url)) {
  const result = await fetchRuntime();
  console.log(`Node ${result.version} 运行时已就绪：${result.target}`);
  console.log(`SHA-256：${result.sha256}`);
}
