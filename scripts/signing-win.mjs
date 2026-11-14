// Authenticode signing for the Windows package. HID access and global key
// hooks make an unsigned NuPhyLab.exe a SmartScreen / antivirus target, so
// signing is the distribution gate: package-win.mjs refuses to ship a public
// artifact without either a certificate or an explicit opt-out.
//
// Certificate sources, in order: a PFX file (AIR_STUDIO_SIGN_PFX +
// AIR_STUDIO_SIGN_PFX_PASSWORD), an explicit hash or subject
// (AIR_STUDIO_SIGN_IDENTITY) from the user stores, or — when the stores hold
// exactly one code-signing certificate — that one. Skipping needs
// AIR_STUDIO_ALLOW_UNSIGNED=1, mirroring the macOS ad-hoc opt-in.
import { execFile } from "node:child_process";
import { access, readdir } from "node:fs/promises";
import { constants } from "node:fs";
import { promisify } from "node:util";
import path from "node:path";
import { fileURLToPath } from "node:url";

const exec = promisify(execFile);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** Windows Kits ship signtool under versioned directories; newest wins. */
export async function findSignTool({
  kitsRoot = process.env["ProgramFiles(x86)"] ? path.join(process.env["ProgramFiles(x86)"], "Windows Kits", "10", "bin") : null,
  explicit = process.env.AIR_STUDIO_SIGNTOOL,
  probe = async (file) => { try { await access(file, constants.X_OK); return true; } catch { return false; } },
  where = () => exec("where.exe", ["signtool"]),
} = {}) {
  if (explicit) return explicit;
  let onPath = null;
  try {
    const { stdout } = await where();
    onPath = String(stdout ?? "").split(/\r?\n/).map((line) => line.trim()).filter(Boolean)[0] ?? null;
  } catch {
    // Not on PATH; fall through to the Windows Kits layout.
  }
  if (onPath) return onPath;
  if (!kitsRoot) return null;
  let entries;
  try { entries = await readdir(kitsRoot); } catch { return null; }
  for (const candidate of versionsFrom(entries)) {
    const tool = path.join(kitsRoot, candidate.version, candidate.arch, "signtool.exe");
    if (await probe(tool)) return tool;
  }
  return null;
}
function versionsFrom(entries) {
  const key = (version) => version.split('.').map(Number);
  return entries
    .filter((entry) => /^\d+\.\d+\.\d+\.\d+$/.test(entry))
    .sort((a, b) => {
      const [first, second] = [key(a), key(b)];
      for (let index = 0; index < 4; index += 1) {
        if (first[index] !== second[index]) return second[index] - first[index];
      }
      return 0;
    })
    .flatMap((version) => ["x64", "arm64", "x86"].map((arch) => ({ version, arch })));
}
/**
 * Picks a certificate from PowerShell's view of the user/machine stores.
 * Only certificates that can actually sign (CodeSigning EKU, private key)
 * qualify; everything else would make signtool fail with a confusing error.
 */
export function selectCertificate(storeListing, requested = process.env.AIR_STUDIO_SIGN_IDENTITY) {
  const parse = (text) => [...text.matchAll(/^\s*([A-F0-9]{40})\s+(\S.*?)\s*$/gm)]
    .map(([, hash, subject]) => ({ hash, subject }));
  const identities = parse(storeListing).filter((entry) => !requested || entry.hash === requested || entry.subject === requested);
  if (requested) {
    if (identities.length !== 1)
      throw new Error(`指定的代码签名证书（${requested}）在证书存储中不存在或不唯一。`);
    return identities[0];
  }
  const unique = [...new Map(identities.map((entry) => [entry.hash, entry])).values()];
  if (unique.length === 1) return unique[0];
  throw new Error(
    unique.length === 0
      ? "证书存储中没有可用于代码签名的证书。请安装证书后重试，或设置 AIR_STUDIO_SIGN_IDENTITY 指定 SHA-1 指纹/主题。"
      : "证书存储中存在多个代码签名证书，请设置 AIR_STUDIO_SIGN_IDENTITY 指定其中一个（SHA-1 指纹或主题）。",
  );
}
export const CERT_ENUM_COMMAND = [
  "-NoProfile", "-NonInteractive", "-Command",
  "Get-ChildItem Cert:\\CurrentUser\\My, Cert:\\LocalMachine\\My | Where-Object { $_.HasPrivateKey -and ($_.Extensions | Where-Object { $_.Oid.FriendlyName -eq 'Key Usage' }) -and ($_.EnhancedKeyUsageList.FriendlyName -contains 'Code Signing') } | Format-Table -HideTableHeaders -AutoSize -Property Thumbprint, Subject",
];
export async function listCertificates({ run = exec, command = CERT_ENUM_COMMAND } = {}) {
  const { stdout } = await run("powershell.exe", command, { timeout: 30_000, maxBuffer: 1_048_576 });
  return String(stdout ?? "");
}
/** Builds one signtool invocation per file; kept pure for tests. */
export function signArguments(tool, certificate, files, { timestamp = process.env.AIR_STUDIO_SIGN_TIMESTAMP !== "0" } = {}) {
  const base = certificate.pfx
    ? ["sign", "/fd", "SHA256", "/f", certificate.pfx, "/p", certificate.pfxPassword ?? ""]
    : ["sign", "/fd", "SHA256", "/sha1", certificate.hash];
  const stamp = timestamp ? ["/tr", process.env.AIR_STUDIO_SIGN_TSA ?? "http://timestamp.digicert.com", "/td", "SHA256"] : [];
  return files.map((file) => [tool, [...base, ...stamp, file]]);
}
export function verifyArguments(tool, files) {
  return files.map((file) => [tool, ["verify", "/pa", "/all", file]]);
}
export const UNSIGNED_IDENTITY = Object.freeze({ name: "unsigned（未签名）", unsigned: true });
/**
 * Resolves how the package will be signed. Never signs anything here —
 * package-win.mjs drives the returned handle once artifacts are in place.
 */
export async function signingHandle({
  allowUnsigned = process.env.AIR_STUDIO_ALLOW_UNSIGNED === "1",
  pfx = process.env.AIR_STUDIO_SIGN_PFX,
  pfxPassword = process.env.AIR_STUDIO_SIGN_PFX_PASSWORD,
  requested = process.env.AIR_STUDIO_SIGN_IDENTITY,
  signTool = findSignTool,
  certificates = listCertificates,
} = {}) {
  const tool = await signTool();
  if (!tool) {
    if (allowUnsigned) return { ...UNSIGNED_IDENTITY, tool: null };
    throw new Error(
      "找不到 signtool（Windows SDK）。请安装 Windows SDK 或设置 AIR_STUDIO_SIGNTOOL；" +
        "开发构建可用 AIR_STUDIO_ALLOW_UNSIGNED=1 跳过签名，但该产物不能对外分发。",
    );
  }
  if (pfx) return { tool, pfx, pfxPassword, name: `PFX:${path.basename(pfx)}`, unsigned: false };
  const listing = await certificates();
  return { tool, ...selectCertificate(listing, requested), unsigned: false };
}
export async function signAll(handle, files, { run = exec } = {}) {
  if (handle.unsigned) return [];
  for (const [tool, args] of signArguments(handle.tool, handle, files)) {
    await run(tool, args, { timeout: 120_000, maxBuffer: 1_048_576 });
  }
  for (const [tool, args] of verifyArguments(handle.tool, files)) {
    await run(tool, args, { timeout: 60_000, maxBuffer: 1_048_576 });
  }
  return files;
}
export function isDirectRun(moduleURL, entry = process.argv[1]) {
  return Boolean(entry) && fileURLToPath(moduleURL) === path.resolve(entry);
}
if (isDirectRun(import.meta.url)) {
  const handle = await signingHandle();
  console.log(`signtool: ${handle.tool ?? "（未找到）"}`);
  console.log(`certificate: ${handle.name ?? handle.subject} (${handle.hash ?? "n/a"})`);
}
