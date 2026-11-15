#!/usr/bin/env node
// Prepares the Windows update feed (updates/appcast-win.xml) for the in-app
// updater: computes the setup.exe SHA-256, signs the installer bytes with
// Ed25519 (Sparkle-style EdDSA) and keeps the newest item first, preserving
// the historical items.
//
// Zero npm dependencies — node:crypto ships Ed25519.
//
// Key workflow (release machine only, never commit the private seed):
//   node scripts/prepare-appcast-win.mjs --generate-key
//     → prints AIR_STUDIO_UPDATE_SIGNING_KEY=<base64 32B seed>  (keep secret,
//       e.g. in the release machine's environment)
//     → prints the matching public key to embed as UPDATE_PUBLIC_KEY in
//       native-win/shell/src/updater/crypto.rs
//
// Feed build:
//   AIR_STUDIO_UPDATE_SIGNING_KEY=<seed> node scripts/prepare-appcast-win.mjs \
//     release/win/NuPhyLab-win-x64-v1.0.13-setup.exe \
//     --notes docs/RELEASE_1.0.13.md
// Notes format matches the Mac feed (NUPHY-RELEASE-NOTES 1 / version: / [zh-CN] / [en]);
// --notes-zh/--notes-en assemble it inline instead of a file.
import { readFile, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, sign, verify } from "node:crypto";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const PKCS8_ED25519_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");

function usage() {
  console.log(`Usage:
  node scripts/prepare-appcast-win.mjs --generate-key
  AIR_STUDIO_UPDATE_SIGNING_KEY=<base64> node scripts/prepare-appcast-win.mjs <setup.exe> [options]

Options:
  --version <x.y.z>       App version (default: package.json, must match the file name)
  --notes <file>          Release notes file (NUPHY-RELEASE-NOTES format, or [zh-CN]/[en] sections)
  --notes-zh <text>       Chinese notes (alternative to --notes)
  --notes-en <text>       English notes (alternative to --notes)
  --feed <file>           Output feed (default: updates/appcast-win.xml)
  --url <url>             Download URL for this release (default: GitHub releases/download/v<version>/…)
`);
}

function fail(message) {
  console.error(`prepare-appcast-win: ${message}`);
  process.exit(1);
}

function signingKey(seedBase64) {
  const seed = Buffer.from(seedBase64 ?? "", "base64");
  if (seed.length !== 32) fail("AIR_STUDIO_UPDATE_SIGNING_KEY must decode to exactly 32 bytes.");
  const privateKey = createPrivateKey({ key: Buffer.concat([PKCS8_ED25519_PREFIX, seed]), format: "der", type: "pkcs8" });
  const publicKey = createPublicKey(privateKey);
  const raw = publicKey.export({ type: "spki", format: "der" });
  return { privateKey, publicKeyBase64: Buffer.from(raw.subarray(raw.length - 32)).toString("base64") };
}

function generateKey() {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const der = privateKey.export({ type: "pkcs8", format: "der" });
  const seed = Buffer.from(der.subarray(der.length - 32)).toString("base64");
  const raw = publicKey.export({ type: "spki", format: "der" });
  console.log("Keep the private seed out of the repository; it only lives on the release machine:");
  console.log(`AIR_STUDIO_UPDATE_SIGNING_KEY=${seed}`);
  console.log("");
  console.log("Embed this verifying key as UPDATE_PUBLIC_KEY in native-win/shell/src/updater/crypto.rs:");
  console.log(Buffer.from(raw.subarray(raw.length - 32)).toString("base64"));
}

function escapeXml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function buildNotes({ notesFile, notesZh, notesEn, version }) {
  if (notesFile) {
    if (!existsSync(notesFile)) fail(`notes file not found: ${notesFile}`);
    const body = readFile(notesFile, "utf8").then(text => text.replace(/\r\n/g, "\n").trimEnd());
    return body;
  }
  if (notesZh || notesEn) {
    return Promise.resolve(
      `NUPHY-RELEASE-NOTES 1\nversion:${version}\n[zh-CN]\n${(notesZh || "").trim()}\n[en]\n${(notesEn || "").trim()}\n`,
    );
  }
  return null;
}

/** Splits an existing feed into (header, items[], footer) keeping item blocks verbatim. */
function splitFeed(xml) {
  const items = [];
  const regex = /<item>[\s\S]*?<\/item>/g;
  let match;
  while ((match = regex.exec(xml))) items.push(match[0]);
  const headEnd = xml.indexOf("<item>");
  const head = headEnd === -1 ? xml : xml.slice(0, headEnd);
  const tailStart = xml.lastIndexOf("</item>");
  const tail = tailStart === -1 ? "" : xml.slice(tailStart + "</item>".length);
  return { head, items, tail };
}

function itemBlock({ version, url, length, signature, sha256, minimumSystemVersion, notes }) {
  const published = new Date().toUTCString().replace("GMT", "+0000");
  return `    <item>
        <title>${escapeXml(version)}</title>
        <pubDate>${published}</pubDate>
        <link>https://github.com/bohu8264/NuphyLab-Win</link>
        <sparkle:version>${escapeXml(version)}</sparkle:version>
        <sparkle:shortVersionString>${escapeXml(version)}</sparkle:shortVersionString>
        <sparkle:minimumSystemVersion>${escapeXml(minimumSystemVersion)}</sparkle:minimumSystemVersion>
        <description sparkle:format="plain-text"><![CDATA[${notes}]]></description>
        <enclosure url="${escapeXml(url)}" length="${length}" type="application/octet-stream" sparkle:edSignature="${signature}" sparkle:sha256="${sha256}"/>
    </item>`;
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv[0] === "--generate-key") return generateKey();

  const artifact = argv[0];
  if (!artifact || artifact.startsWith("--")) {
    usage();
    fail("a setup.exe path is required (or use --generate-key)");
  }
  const options = {};
  for (let index = 1; index < argv.length; index += 2) {
    const flag = argv[index];
    if (!flag.startsWith("--") || index + 1 >= argv.length) fail(`malformed argument: ${flag}`);
    options[flag.slice(2)] = argv[index + 1];
  }
  const { version: packageVersion } = JSON.parse(await readFile(path.join(root, "package.json"), "utf8"));
  const version = options.version ?? packageVersion;
  if (!/^\d+\.\d+\.\d+$/.test(version)) fail(`invalid version: ${version}`);
  const artifactPath = path.resolve(artifact);
  if (!path.basename(artifactPath).includes(version)) {
    fail(`artifact file name must include the release version (${version})`);
  }
  const feedPath = path.resolve(root, options.feed ?? path.join("updates", "appcast-win.xml"));
  const url = options.url
    ?? `https://github.com/bohu8264/NuphyLab-Win/releases/download/v${version}/${path.basename(artifactPath)}`;

  const bytes = await readFile(artifactPath);
  const sha256 = createHash("sha256").update(bytes).digest("hex");
  const { privateKey, publicKeyBase64 } = signingKey(process.env.AIR_STUDIO_UPDATE_SIGNING_KEY);
  const signature = sign(null, bytes, privateKey).toString("base64");

  let notes = await buildNotes({ notesFile: options.notes, notesZh: options["notes-zh"], notesEn: options["notes-en"], version });
  if (!notes) fail("release notes are required: pass --notes <file> or --notes-zh/--notes-en");
  if (!/^NUPHY-RELEASE-NOTES/.test(notes)) notes = `NUPHY-RELEASE-NOTES 1\nversion:${version}\n${notes}`;
  if (!notes.includes("[zh-CN]") || !notes.includes("[en]")) fail("release notes must contain [zh-CN] and [en] sections");
  if (notes.includes("]]>")) fail("release notes must not contain ']]>' (CDATA terminator)");

  const head = `<?xml version="1.0" standalone="yes"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
    <channel>
        <title>NuPhy Lab (Windows)</title>
        <link>https://github.com/bohu8264/NuphyLab-Win</link>
        <description>Windows update feed for the NuPhyLab in-app updater.</description>
`;
  const tail = `    </channel>
</rss>
`;

  const existing = existsSync(feedPath) ? await readFile(feedPath, "utf8") : "";
  const { items } = splitFeed(existing);
  const kept = items.filter(item => !/<sparkle:version>([^<]*)<\/sparkle:version>/.exec(item)?.[1]?.startsWith(version));
  const block = itemBlock({
    version,
    url,
    length: bytes.length,
    signature,
    sha256,
    minimumSystemVersion: "10.0",
    notes,
  });
  await writeFile(feedPath, head + block + "\n" + kept.join("\n") + (kept.length ? "\n" : "") + tail);

  // Self-check: verify the just-produced signature the same way the shell will.
  const ok = verify(null, bytes, createPublicKey(privateKey), Buffer.from(signature, "base64"));
  if (!ok) fail("self-verification failed — the feed was NOT updated correctly");
  console.log(`Feed updated: ${feedPath}`);
  console.log(`  version   : ${version}`);
  console.log(`  size      : ${bytes.length} bytes`);
  console.log(`  sha256    : ${sha256}`);
  console.log(`  signature : ${signature.slice(0, 24)}…`);
  console.log(`  public key: ${publicKeyBase64} (UPDATE_PUBLIC_KEY in crypto.rs must match)`);
  console.log(`  items kept: ${kept.length}`);
}

await main();
