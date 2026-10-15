param(
    [Parameter(Mandatory = $true)]
    [string] $ProductDirectory
)

$ErrorActionPreference = "Stop"
$backupDirectory = Join-Path $env:LOCALAPPDATA (Join-Path $ProductDirectory "protocol-backups")
if (-not (Test-Path -LiteralPath $backupDirectory -PathType Container)) {
    exit 0
}

$classes = [Microsoft.Win32.Registry]::CurrentUser
foreach ($ownerFile in Get-ChildItem -LiteralPath $backupDirectory -Filter "*.owner" -File) {
    $protocol = [IO.Path]::GetFileNameWithoutExtension($ownerFile.Name)
    $owner = (Get-Content -LiteralPath $ownerFile.FullName -Raw -Encoding UTF8).Trim()

    if ($protocol -eq "bplist") {
        $extensionKey = $classes.OpenSubKey("Software\Classes\.bplist")
        if ($null -eq $extensionKey) {
            continue
        }

        try {
            $currentProgId = [string] $extensionKey.GetValue("")
        }
        finally {
            $extensionKey.Dispose()
        }

        if (-not [string]::Equals($currentProgId, "BSManager.BeatSaberPlaylist", [StringComparison]::OrdinalIgnoreCase)) {
            continue
        }

        $commandKey = $classes.OpenSubKey("Software\Classes\BSManager.BeatSaberPlaylist\shell\open\command")
        if ($null -eq $commandKey) {
            continue
        }

        try {
            $currentCommand = [string] $commandKey.GetValue("")
        }
        finally {
            $commandKey.Dispose()
        }

        $expectedCommand = '"' + $owner + '" "%1"'
        if (-not [string]::Equals($currentCommand, $expectedCommand, [StringComparison]::OrdinalIgnoreCase)) {
            continue
        }

        $classes.DeleteSubKeyTree("Software\Classes\.bplist", $false)
        $classes.DeleteSubKeyTree("Software\Classes\BSManager.BeatSaberPlaylist", $false)
        foreach ($backupName in @("bplist-progid.reg", "bplist-extension.reg")) {
            $registryBackup = Join-Path $backupDirectory $backupName
            if (-not (Test-Path -LiteralPath $registryBackup -PathType Leaf)) {
                continue
            }
            $restore = Start-Process -FilePath "$env:WINDIR\System32\reg.exe" `
                -ArgumentList @("import", "`"$registryBackup`"") -Wait -PassThru -NoNewWindow
            if ($restore.ExitCode -ne 0) {
                throw "Could not restore the previous .bplist file association. Backup: $registryBackup"
            }
        }
        continue
    }

    $commandPath = "Software\Classes\$protocol\shell\open\command"
    $commandKey = $classes.OpenSubKey($commandPath)
    if ($null -eq $commandKey) {
        continue
    }

    try {
        $currentCommand = [string] $commandKey.GetValue("")
    }
    finally {
        $commandKey.Dispose()
    }

    $expectedCommand = '"' + $owner + '" "%1"'
    if (-not [string]::Equals($currentCommand, $expectedCommand, [StringComparison]::OrdinalIgnoreCase)) {
        continue
    }

    $classes.DeleteSubKeyTree("Software\Classes\$protocol", $false)
    $registryBackup = Join-Path $backupDirectory ($protocol + ".reg")
    if (Test-Path -LiteralPath $registryBackup -PathType Leaf) {
        $restore = Start-Process -FilePath "$env:WINDIR\System32\reg.exe" `
            -ArgumentList @("import", "`"$registryBackup`"") -Wait -PassThru -NoNewWindow
        if ($restore.ExitCode -ne 0) {
            throw "Could not restore the previous $protocol registration. Backup: $registryBackup"
        }
    }
}
