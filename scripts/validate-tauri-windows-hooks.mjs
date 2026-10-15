import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

if (process.platform !== "win32") {
    throw new Error("Windows installer hook validation requires Windows PowerShell.");
}

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const files = [
    path.join(root, "src-tauri", "windows", "replacement-preinstall.ps1"),
    path.join(root, "src-tauri", "windows", "restore-protocols.ps1"),
];
const quotePowerShell = value => `'${value.replaceAll("'", "''")}'`;
const script = [
    "$ErrorActionPreference = 'Stop'",
    `$paths = @(${files.map(quotePowerShell).join(", ")})`,
    "foreach ($path in $paths) {",
    "    $tokens = $null",
    "    $parseErrors = $null",
    "    [System.Management.Automation.Language.Parser]::ParseFile($path, [ref] $tokens, [ref] $parseErrors) | Out-Null",
    "    foreach ($parseError in $parseErrors) {",
    "        [Console]::Error.WriteLine('{0}:{1}: {2}', $path, $parseError.Extent.StartLineNumber, $parseError.Message)",
    "    }",
    "    if ($parseErrors.Count -gt 0) { exit 1 }",
    "}",
    "Write-Output ('PowerShell hooks parsed: ' + ($paths -join ', '))",
].join("\n");
const encodedScript = Buffer.from(script, "utf16le").toString("base64");
const powershell = path.join(process.env.SystemRoot ?? "C:\\Windows", "System32", "WindowsPowerShell", "v1.0", "powershell.exe");
const result = spawnSync(powershell, ["-NoProfile", "-NonInteractive", "-EncodedCommand", encodedScript], { encoding: "utf8" });

if (result.error) throw result.error;
if (result.status !== 0) {
    process.stderr.write(result.stdout ?? "");
    process.stderr.write(result.stderr ?? "");
    throw new Error(`Windows installer hook syntax validation failed with exit code ${result.status}.`);
}

process.stdout.write(result.stdout ?? "");
