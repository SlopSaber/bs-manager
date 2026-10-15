import { createHash } from "node:crypto";
import { copyFile, mkdir, readdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const profiles = {
    preview: {
        productName: "BSManager Preview",
        identifier: "io.bsmanager.bsmanager.preview",
    },
    replacement: {
        productName: "BSManager",
        identifier: "io.bsmanager.bsmanager",
    },
};

const profileName = process.argv[2];
const profile = profiles[profileName];
if (!profile) throw new Error("Pass either 'preview' or 'replacement'.");
if (process.platform !== "win32") throw new Error("Windows installer collection requires Windows.");

const bundleDirectory = path.join(root, "src-tauri", "target", "x86_64-pc-windows-msvc", "release", "bundle", "nsis");
const files = await readdir(bundleDirectory);
const sourceName = `${profile.productName}_1.6.0_x64-setup.exe`;
if (!files.includes(sourceName)) throw new Error(`Installer not found: ${path.join(bundleDirectory, sourceName)}.`);

const source = path.join(bundleDirectory, sourceName);
const outputDirectory = path.join(root, "release", "tauri", profileName);
const outputName = `BSManager-${profileName === "preview" ? "Preview-" : ""}1.6.0-x64-setup.exe`;
const output = path.join(outputDirectory, outputName);

await mkdir(outputDirectory, { recursive: true });
await copyFile(source, output);
const digest = createHash("sha256");
const { createReadStream } = await import("node:fs");
for await (const chunk of createReadStream(output)) digest.update(chunk);
await writeFile(path.join(outputDirectory, "artifact.json"), `${JSON.stringify({
    profile: profileName,
    productName: profile.productName,
    identifier: profile.identifier,
    version: "1.6.0",
    installer: outputName,
    sha256: digest.digest("hex"),
    signed: false,
}, null, 2)}\n`);
console.log(`Installer: ${output}`);
