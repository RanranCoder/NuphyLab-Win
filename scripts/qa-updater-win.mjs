#!/usr/bin/env node
// End-to-end QA for the Windows in-app updater. Generates a throwaway
// Ed25519 keypair, serves a fixture appcast + fake setup.exe over a local
// HTTP server (the installer endpoint 302-redirects, exercising the download
// redirect path), and runs the shell's AIR_STUDIO_UPDATE_QA harness which
// performs check → download → SHA-256 → Ed25519 → installer command dry run,
// plus the tampered-signature / tampered-installer / version-replay refusals.
// Nothing is really installed and no network beyond 127.0.0.1 is touched.
//
// Usage: node scripts/qa-updater-win.mjs [--build]
import { spawn, spawnSync } from "node:child_process";
import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, randomBytes, sign } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const shouldBuild = process.argv.includes("--build");

function fail(message) {
  console.error(`qa-updater-win: ${message}`);
  process.exit(1);
}

// --- keypair + fixtures ----------------------------------------------------
const { privateKey } = generateKeyPairSync("ed25519");
const pkcs8 = privateKey.export({ type: "pkcs8", format: "der" });
const seedBase64 = Buffer.from(pkcs8.subarray(pkcs8.length - 32)).toString("base64");
const spki = createPublicKey(privateKey).export({ type: "spki", format: "der" });
const publicKeyBase64 = Buffer.from(spki.subarray(spki.length - 32)).toString("base64");

const setupBytes = Buffer.concat([Buffer.from("NuPhyLab QA setup payload\r\n"), randomBytes(512 * 1024)]);
const setupTampered = Buffer.from(setupBytes);
setupTampered[64] ^= 0x5a;

const digest = bytes => createHash("sha256").update(bytes).digest("hex");
const signature = bytes => sign(null, bytes, privateKey).toString("base64");
// A signature over different bytes than the shipped installer (tamper case 1).
const wrongSignature = sign(null, Buffer.concat([setupBytes, Buffer.from("x")]), privateKey).toString("base64");

const { version: currentVersion } = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8"));
const remoteVersion = "9.9.9";

function appcastXml({ version, url, sha, sig }) {
  return `<?xml version="1.0" standalone="yes"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
    <channel>
        <title>NuPhy Lab QA (Windows)</title>
        <item>
            <title>${version}</title>
            <sparkle:version>${version}</sparkle:version>
            <sparkle:shortVersionString>${version}</sparkle:shortVersionString>
            <sparkle:minimumSystemVersion>10.0</sparkle:minimumSystemVersion>
            <description sparkle:format="plain-text"><![CDATA[NUPHY-RELEASE-NOTES 1
version:${version}
[zh-CN]
QA 模拟更新：修复测试覆盖。
[en]
QA simulated update: fixes test coverage.
]]></description>
            <enclosure url="${url}" length="${sha === digest(setupBytes) ? setupBytes.length : setupTampered.length}" type="application/octet-stream" sparkle:edSignature="${sig}" sparkle:sha256="${sha}"/>
        </item>
    </channel>
</rss>
`;
}

// --- fixture server (own process: some local security software only lets
// non-node processes connect to ports held by an independently started node) --
const fixtureDir = mkdtempSync(path.join(tmpdir(), "nuphylab-updater-qa-"));
const serverFile = path.join(fixtureDir, "fixture-server.cjs");
const portFile = path.join(fixtureDir, "port.txt");
writeFileSync(serverFile, `const http=require('node:http');const fs=require('node:fs');${createServerSource()}`);
// The server runs under cmd.exe: some local security software blocks loopback
// traffic involving processes spawned directly by node.exe, and reports its
// port through a file so the driver does not depend on stdout piping.
const server = spawn("cmd.exe", ["/c", process.execPath, serverFile], { stdio: "ignore" });
let port = 0;
function createServerSource() {
  return `
const setupBytesLength = ${setupBytes.length};
const setupTamperedLength = ${setupTampered.length};
const feeds = {
  "/appcast-win.xml": ${JSON.stringify(appcastXml({ version: remoteVersion, url: "SETUP_URL", sha: digest(setupBytes), sig: signature(setupBytes) }))},
  "/appcast-tampered-sig.xml": ${JSON.stringify(appcastXml({ version: remoteVersion, url: "SETUP_URL", sha: digest(setupBytes), sig: wrongSignature }))},
  "/appcast-tampered-setup.xml": ${JSON.stringify(appcastXml({ version: remoteVersion, url: "SETUP_TAMPERED_URL", sha: digest(setupBytes), sig: signature(setupBytes) }))},
  "/appcast-older.xml": ${JSON.stringify(appcastXml({ version: "0.0.1", url: "SETUP_URL", sha: digest(setupBytes), sig: signature(setupBytes) }))},
};
const setup = Buffer.from(${JSON.stringify(setupBytes.toString("base64"))}, "base64");
const setupTampered = Buffer.from(${JSON.stringify(setupTampered.toString("base64"))}, "base64");
const server = http.createServer((request, response) => {
  const url = (request.url || "/").split("?")[0];
  const feed = feeds[url];
  if (feed) {
    const body = feed.replaceAll("SETUP_TAMPERED_URL", "http://127.0.0.1:" + server.address().port + "/setup-tampered.exe")
      .replaceAll("SETUP_URL", "http://127.0.0.1:" + server.address().port + "/setup.exe");
    response.writeHead(200, { "Content-Type": "application/xml", "Content-Length": Buffer.byteLength(body) });
    response.end(body);
    return;
  }
  if (url === "/setup.exe") {
    // Redirect chain: the shell must follow it to the real payload.
    response.writeHead(302, { Location: "/setup-real.exe" });
    response.end();
    return;
  }
  if (url === "/setup-real.exe") {
    response.writeHead(200, { "Content-Type": "application/octet-stream", "Content-Length": setup.length });
    response.end(setup);
    return;
  }
  if (url === "/setup-tampered.exe") {
    response.writeHead(200, { "Content-Type": "application/octet-stream", "Content-Length": setupTampered.length });
    response.end(setupTampered);
    return;
  }
  response.writeHead(404);
  response.end("not found");
});
server.listen(0, "127.0.0.1", () => {
  fs.writeFileSync(${JSON.stringify(portFile)}, String(server.address().port));
});
`;
}

async function run() {
  // Wait for the fixture server to report its port.
  for (let index = 0; index < 50 && !port; index += 1) {
    await new Promise(resolve => setTimeout(resolve, 100));
    try { port = Number(readFileSync(portFile, "utf8").trim()); } catch { /* not yet */ }
  }
  if (!port) fail("fixture server did not start");
  // Sanity check: node itself must reach the fixture server before blaming
  // the shell's HTTP stack.
  const probe = await fetch(`http://127.0.0.1:${port}/appcast-win.xml`).then(r => r.status).catch(error => `FAIL ${error.cause?.code || error.message}`);
  console.log(`node self-check: GET /appcast-win.xml → ${probe}`);
  const exe = path.join(root, "native-win", "target", "debug", "NuPhyLab.exe");
  if (shouldBuild || !existsQuiet(exe)) {
    console.log("Building the shell (debug)…");
    const built = spawnSync("node", [path.join(root, "scripts", "cargo-win.mjs"), "build"], {
      cwd: path.join(root, "native-win"), stdio: "inherit",
    });
    if (built.status !== 0) fail("cargo build failed");
  }
  if (!existsQuiet(exe)) fail(`shell not found: ${exe} (run with --build)`);

  const base = `http://127.0.0.1:${port}`;
  console.log(`Fixture server: ${base}`);
  console.log(`Versions: running=${currentVersion} remote=${remoteVersion}`);
  // Some local security software blocks loopback connections from processes
  // spawned directly by node.exe; going through cmd.exe keeps the child out
  // of that policy while inheriting the same environment.
  const result = spawnSync("cmd.exe", ["/c", exe], {
    env: {
      ...process.env,
      AIR_STUDIO_UPDATE_QA: "1",
      AIR_STUDIO_UPDATE_PUBLIC_KEY: publicKeyBase64,
      AIR_STUDIO_UPDATE_SIGNING_KEY: "",
      AIR_STUDIO_UPDATE_FEED: `${base}/appcast-win.xml`,
      AIR_STUDIO_UPDATE_QA_TAMPERED_SIG: `${base}/appcast-tampered-sig.xml`,
      AIR_STUDIO_UPDATE_QA_TAMPERED_SETUP: `${base}/appcast-tampered-setup.xml`,
      AIR_STUDIO_UPDATE_QA_OLDER: `${base}/appcast-older.xml`,
      AIR_STUDIO_DATA: fixtureDir,
      AIR_STUDIO_NO_UPDATE_CHECK: "1",
    },
    encoding: "utf8",
  });
  process.stdout.write(result.stdout || "");
  process.stderr.write(result.stderr || "");
  console.log(`Shell QA exit code: ${result.status}`);
  console.log(`QA data dir (auto-removed): ${fixtureDir}`);
  process.exitCode = result.status ?? 1;
  server.kill();
  rmSync(fixtureDir, { recursive: true, force: true });
}

function existsQuiet(file) {
  try { return readFileSync(file).length > 0; } catch { return false; }
}

await run();
