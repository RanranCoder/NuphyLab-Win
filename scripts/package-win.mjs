// Assembles the Windows distribution and produces the portable zip and — when
// Inno Setup is installed — the setup executable. Mirrors scripts/package.mjs
// for the flat Windows layout the Rust shell resolves next to NuPhyLab.exe:
//   NuPhyLab.exe  WebView2Loader.dll  runtime/node.exe  bin/HIDBridge.exe
//   server/ shared/ hooks/ ui/dist/  signing.json
import { cp, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { runtimePath, verifyRuntime, isDirectRun } from "./fetch-node.mjs";
import { signingHandle, signAll } from "./signing-win.mjs";

const exec = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
export const outputRoot = path.join(root, "release", "win");
// Files that must exist before a package may ship. `dir` entries need at
// least one entry; `file` entries must be readable.
export const LAYOUT_SPEC = [
  { kind: "file", path: "NuPhyLab.exe" },
  { kind: "file", path: "WebView2Loader.dll" },
  { kind: "file", path: "runtime/node.exe" },
  { kind: "file", path: "runtime/LICENSE.node" },
  { kind: "file", path: "bin/HIDBridge.exe" },
  { kind: "dir", path: "server" },
  { kind: "dir", path: "shared" },
  { kind: "dir", path: "hooks" },
  { kind: "dir", path: "assets/typing-sounds" },
  { kind: "dir", path: "ui/dist" },
];
/** Returns the missing/incomplete entries of a packaged layout. */
export async function verifyLayout(dir, spec = LAYOUT_SPEC, { list = readdir } = {}) {
  const problems = [];
  for (const entry of spec) {
    const target = path.join(dir, ...entry.path.split("/"));
    try {
      if (entry.kind === "file") {
        const handle = await readFile(target);
        if (handle.length === 0) problems.push(`${entry.path} 是空文件`);
      } else if ((await list(target)).length === 0) {
        problems.push(`${entry.path} 是空目录`);
      }
    } catch {
      problems.push(`${entry.path} 缺失`);
    }
  }
  return problems;
}
export async function findIscc({
  explicit = process.env.AIR_STUDIO_ISCC,
  programFiles = process.env["ProgramFiles(x86)"],
  programFiles64 = process.env.ProgramFiles,
  probe = async (file) => { try { await readFile(file); return true; } catch { return false; } },
} = {}) {
  if (explicit) return explicit;
  const candidates = [
    programFiles && path.join(programFiles, "Inno Setup 6", "ISCC.exe"),
    programFiles64 && path.join(programFiles64, "Inno Setup 6", "ISCC.exe"),
  ].filter(Boolean);
  for (const candidate of candidates)
    if (await probe(candidate)) return candidate;
  return null;
}
/** Builds the ISCC argument list. OutputBaseFilename must arrive without an
 *  ".exe" suffix — ISCC appends the extension itself, and a doubled
 *  "…-setup.exe.exe" is what the first real installer build produced. */
export function isccArgs({ issFile, layout, output, version, installerFile }) {
  return [
    issFile,
    `/DAppVersion=${version}`,
    `/DSourceDir=${layout}`,
    `/DOutputDir=${output}`,
    `/DOutputFile=${path.basename(installerFile, ".exe")}`,
    "/Qp",
  ];
}
async function zipLayout(layout, zipFile) {
  // Compress-Archive keeps the directory structure; tar would need a flag
  // matrix across Windows builds for no gain here.
  await exec("powershell.exe", [
    "-NoProfile", "-NonInteractive", "-Command",
    `Compress-Archive -Path '${path.join(layout, "*")}' -DestinationPath '${zipFile}' -Force`,
  ], { timeout: 300_000, maxBuffer: 1_048_576 });
}
export async function packageWindows({
  version,
  output = outputRoot,
  runtime = process.env.AIR_STUDIO_NODE,
  iscc = findIscc,
  sign = signAll,
  verify = verifyLayout,
  run = exec,
} = {}) {
  if (!version) ({ version } = JSON.parse(await readFile(path.join(root, "package.json"), "utf8")));
  if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error("Invalid app version.");
  const layout = path.join(output, "NuPhyLab-win-x64");
  await rm(layout, { recursive: true, force: true });
  await mkdir(path.join(layout, "runtime"), { recursive: true });
  await mkdir(path.join(layout, "bin"), { recursive: true });

  // The bundled runtime must be the official standalone executable: a PATH
  // node may be a symlink farm or ABI-incompatible with the target OS.
  const nodeBinary = runtime ?? runtimePath("x64");
  try {
    await verifyRuntime(nodeBinary);
  } catch {
    throw new Error("找不到可用的 Node 运行时。请运行 npm run fetch-node，或用 AIR_STUDIO_NODE 指定一个。");
  }
  const sourceFiles = [
    ["NuPhyLab.exe", "NuPhyLab.exe"],
    ["WebView2Loader.dll", "WebView2Loader.dll"],
    ["bin/HIDBridge.exe", "bin/HIDBridge.exe"],
    [path.relative(root, nodeBinary), "runtime/node.exe"],
    ["assets/LICENSE.node", "runtime/LICENSE.node"],
    ["README.md", "README.md"],
    ["README.zh-CN.md", "README.zh-CN.md"],
    ["LICENSE", "LICENSE"],
    ["THIRD_PARTY_NOTICES.md", "THIRD_PARTY_NOTICES.md"],
  ];
  for (const [from, to] of sourceFiles) {
    try {
      await cp(path.join(root, from), path.join(layout, to));
    } catch (error) {
      throw new Error(`打包缺少 ${from}（${error.code ?? error.message}）。请先完成对应构建步骤。`);
    }
  }
  // assets/typing-sounds feeds the HIDBridge typing-sounds engine via the
  // shell's AIR_STUDIO_TYPING_ASSETS; it must exist in the repo first.
  for (const name of ["server", "shared", "hooks", path.join("ui", "dist"), path.join("assets", "typing-sounds")])
    await cp(path.join(root, name), path.join(layout, name), { recursive: true });

  // Signing is the distribution gate: refuse to emit a public artifact with
  // no certificate unless the operator opted out explicitly.
  const handle = await signingHandle();
  const signed = await sign(handle, [
    path.join(layout, "NuPhyLab.exe"),
    path.join(layout, "bin", "HIDBridge.exe"),
  ]);
  await writeFile(
    path.join(layout, "signing.json"),
    JSON.stringify({
      version,
      platform: process.platform,
      arch: process.arch,
      adhoc: handle.unsigned === true,
      identity: handle.subject ?? handle.name ?? "",
      packagedAt: new Date().toISOString(),
    }, null, 2),
  );

  const problems = await verify(layout);
  if (problems.length) throw new Error(`打包布局不完整：\n${problems.join("\n")}`);
  // The packaged node must start; this also rejects CPU-mismatched binaries.
  await exec(path.join(layout, "runtime", "node.exe"), ["--version"], { timeout: 30_000 });

  const zipFile = path.join(output, `NuPhyLab-win-x64-v${version}.zip`);
  await rm(zipFile, { force: true });
  await zipLayout(layout, zipFile);

  const installer = await iscc();
  let installerFile = null;
  if (installer) {
    installerFile = path.join(output, `NuPhyLab-win-x64-v${version}-setup.exe`);
    await exec(installer, isccArgs({
      issFile: path.join(root, "scripts", "installer-win.iss"),
      layout,
      output,
      version,
      installerFile,
    }), { timeout: 300_000, maxBuffer: 1_048_576 });
  }

  return {
    layout,
    zipFile,
    installerFile,
    signed: handle.unsigned ? [] : signed,
    identity: handle.subject ?? handle.name ?? "unsigned",
  };
}
if (isDirectRun(import.meta.url)) {
  const result = await packageWindows({});
  console.log("Layout: " + result.layout);
  console.log("Portable zip: " + result.zipFile);
  console.log(result.installerFile ? "Setup: " + result.installerFile : "Setup: 跳过（未找到 Inno Setup 6，安装包未生成）");
  console.log("Signing: " + (result.signed.length ? result.identity : "unsigned（AIR_STUDIO_ALLOW_UNSIGNED=1，请勿对外分发）"));
}
