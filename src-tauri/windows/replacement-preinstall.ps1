param(
    [Parameter(Mandatory = $true)]
    [string] $ProductDirectory
)

$ErrorActionPreference = "Stop"
if ($ProductDirectory -ne "BSManager") {
    exit 0
}

function Get-LegacyInstallations {
    $found = @{}
    foreach ($hive in @([Microsoft.Win32.RegistryHive]::CurrentUser, [Microsoft.Win32.RegistryHive]::LocalMachine)) {
        $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey($hive, [Microsoft.Win32.RegistryView]::Default)
        try {
            $uninstallRoot = $base.OpenSubKey("Software\Microsoft\Windows\CurrentVersion\Uninstall")
            if ($null -eq $uninstallRoot) { continue }
            try {
                foreach ($entryName in $uninstallRoot.GetSubKeyNames()) {
                    $entry = $uninstallRoot.OpenSubKey($entryName)
                    if ($null -eq $entry) { continue }
                    try {
                        if ([string] $entry.GetValue("DisplayName") -ne "BSManager") { continue }
                        $installLocation = [string] $entry.GetValue("InstallLocation")
                        $uninstallString = [string] $entry.GetValue("UninstallString")
                        if ([string]::IsNullOrWhiteSpace($uninstallString)) { continue }
                        if ($uninstallString -match '^\s*"(?<path>[^"]+\.exe)"') {
                            $uninstaller = $Matches.path
                        } elseif ($uninstallString -match '^\s*(?<path>.+?\.exe)(?:\s|$)') {
                            $uninstaller = $Matches.path.Trim('"')
                        } else {
                            continue
                        }
                        if ([string]::IsNullOrWhiteSpace($installLocation)) {
                            $installLocation = Split-Path -Parent $uninstaller
                        }
                        $gameManagerExe = Join-Path $installLocation "BSManager.exe"
                        if (-not (Test-Path -LiteralPath $gameManagerExe -PathType Leaf)) {
                            continue
                        }
                        $found[$gameManagerExe.ToLowerInvariant()] = [pscustomobject]@{
                            Executable = $gameManagerExe
                            Uninstaller = $uninstaller
                            InstallLocation = $installLocation
                            RegistryHive = $hive.ToString()
                            RegistryKey = "Software\Microsoft\Windows\CurrentVersion\Uninstall\$entryName"
                        }
                    }
                    finally {
                        $entry.Dispose()
                    }
                }
            }
            finally {
                if ($null -ne $uninstallRoot) { $uninstallRoot.Dispose() }
            }
        }
        finally {
            $base.Dispose()
        }
    }
    return @($found.Values)
}

function Export-RegistryKey([string] $hiveName, [string] $relativeKey, [string] $destination) {
    $hive = if ($hiveName -eq "HKCU") { [Microsoft.Win32.RegistryHive]::CurrentUser } else { [Microsoft.Win32.RegistryHive]::LocalMachine }
    $base = [Microsoft.Win32.RegistryKey]::OpenBaseKey($hive, [Microsoft.Win32.RegistryView]::Default)
    try {
        $key = $base.OpenSubKey($relativeKey)
        if ($null -eq $key) { return $false }
        $key.Dispose()
    }
    finally {
        $base.Dispose()
    }

    $registryPath = "$hiveName\$relativeKey"
    $result = Start-Process -FilePath "$env:WINDIR\System32\reg.exe" `
        -ArgumentList @("export", "`"$registryPath`"", "`"$destination`"", "/y") `
        -Wait -PassThru -NoNewWindow
    if ($result.ExitCode -ne 0) {
        throw "Could not back up registry key $registryPath"
    }
    return $true
}

function Restore-RegistryBackups([string[]] $backupFiles) {
    foreach ($backupFile in $backupFiles) {
        if (-not (Test-Path -LiteralPath $backupFile -PathType Leaf)) { continue }
        $result = Start-Process -FilePath "$env:WINDIR\System32\reg.exe" `
            -ArgumentList @("import", "`"$backupFile`"") -Wait -PassThru -NoNewWindow
        if ($result.ExitCode -ne 0) {
            Write-Error "Could not restore registry backup $backupFile"
        }
    }
}

function Restore-LegacySettingsIfMissing([string] $backup, [string] $destination) {
    if ([string]::IsNullOrWhiteSpace($backup) -or (Test-Path -LiteralPath $destination -PathType Leaf)) {
        return
    }
    New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
    Copy-Item -LiteralPath $backup -Destination $destination
}

try {
    $legacy = @(Get-LegacyInstallations)
    if ($legacy.Count -eq 0) { exit 0 }
    if ($legacy.Count -gt 1) {
        throw "More than one legacy BSManager installation was found. Remove older copies before installing the replacement."
    }
    $legacy = $legacy[0]
    if (-not (Test-Path -LiteralPath $legacy.Uninstaller -PathType Leaf)) {
        throw "The registered BSManager uninstaller was not found at $($legacy.Uninstaller)."
    }
    if (Get-Process -Name "BSManager" -ErrorAction SilentlyContinue) {
        throw "Close BSManager before installing its replacement."
    }

    $stamp = [DateTime]::UtcNow.ToString("yyyyMMddTHHmmssfffZ")
    $backupDirectory = Join-Path $env:LOCALAPPDATA (Join-Path "BSManager\migration-backups" "$stamp-$([Guid]::NewGuid().ToString('N'))")
    New-Item -ItemType Directory -Path $backupDirectory -Force | Out-Null
    $settingsPath = Join-Path $env:APPDATA "bs-manager\config.json"
    $settingsBackup = $null
    if (Test-Path -LiteralPath $settingsPath -PathType Leaf) {
        $settingsBackup = Join-Path $backupDirectory "config.json"
        Copy-Item -LiteralPath $settingsPath -Destination $settingsBackup
    }

    $registryBackups = @()
    $registryKeys = @("bsmanager", "beatsaver", "web+bsmap", "bsplaylist", "modelsaber", ".bplist")
    foreach ($hiveName in @("HKCU", "HKLM")) {
        foreach ($keyName in $registryKeys) {
            $backupFile = Join-Path $backupDirectory ("$hiveName-" + ($keyName -replace '[^A-Za-z0-9_.+-]', '_') + ".reg")
            if (Export-RegistryKey $hiveName "Software\Classes\$keyName" $backupFile) {
                $registryBackups += $backupFile
            }
        }
    }

    $extensionRoots = @(
        [pscustomobject]@{ Hive = "HKCU"; Root = [Microsoft.Win32.Registry]::CurrentUser },
        [pscustomobject]@{ Hive = "HKLM"; Root = [Microsoft.Win32.Registry]::LocalMachine }
    )
    foreach ($root in $extensionRoots) {
        $classesKey = $root.Root.OpenSubKey("Software\Classes\.bplist")
        if ($null -eq $classesKey) { continue }
        try { $progId = [string] $classesKey.GetValue("") } finally { $classesKey.Dispose() }
        if ([string]::IsNullOrWhiteSpace($progId) -or $progId.StartsWith(".")) { continue }
        $backupFile = Join-Path $backupDirectory ("$($root.Hive)-" + ($progId -replace '[^A-Za-z0-9_.+-]', '_') + ".reg")
        if (Export-RegistryKey $root.Hive "Software\Classes\$progId" $backupFile) {
            $registryBackups += $backupFile
        }
    }

    $details = [ordered]@{
        legacyExecutable = $legacy.Executable
        uninstaller = $legacy.Uninstaller
        registryHive = $legacy.RegistryHive
        registryKey = $legacy.RegistryKey
        settingsPath = $settingsPath
        settingsBackup = $settingsBackup
        registryBackups = $registryBackups
    }
    $details | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $backupDirectory "migration.json") -Encoding UTF8

    $uninstall = Start-Process -FilePath $legacy.Uninstaller -ArgumentList @("/S") `
        -WorkingDirectory $legacy.InstallLocation -Wait -PassThru
    if ($uninstall.ExitCode -ne 0 -or (Test-Path -LiteralPath $legacy.Executable -PathType Leaf)) {
        Restore-RegistryBackups $registryBackups
        Restore-LegacySettingsIfMissing $settingsBackup $settingsPath
        throw "The legacy BSManager uninstaller failed. Replacement install cancelled; backups are in $backupDirectory."
    }
    Restore-LegacySettingsIfMissing $settingsBackup $settingsPath
}
catch {
    Write-Error $_
    exit 1
}

exit 0
