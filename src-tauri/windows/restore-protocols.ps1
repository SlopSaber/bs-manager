param(
    [Parameter(Mandatory = $true)]
    [string] $ProductDirectory
)

$ErrorActionPreference = "Stop"
$backupDirectory = Join-Path $env:LOCALAPPDATA (Join-Path $ProductDirectory "protocol-backups")
$otherProductDirectory = if ($ProductDirectory -eq "BSManager") { "BSManager Preview" } else { "BSManager" }
$otherBackupDirectory = Join-Path $env:LOCALAPPDATA (Join-Path $otherProductDirectory "protocol-backups")
if (-not (Test-Path -LiteralPath $backupDirectory -PathType Container)) {
    exit 0
}

function Get-BackupExecutable([string] $backupFile, [string] $commandKey) {
    if (-not (Test-Path -LiteralPath $backupFile -PathType Leaf)) { return $null }
    $lines = @(Get-Content -LiteralPath $backupFile)
    $section = "[HKEY_CURRENT_USER\$commandKey]"
    $sectionIndex = [Array]::IndexOf([string[]] $lines, $section)
    if ($sectionIndex -lt 0) { return $null }

    for ($index = $sectionIndex + 1; $index -lt $lines.Length; $index++) {
        if ($lines[$index].StartsWith("[")) { break }
        if ($lines[$index] -match '^@="(?<encoded>.*)"$') {
            $command = $Matches.encoded.Replace('\\', '\').Replace('\"', '"')
            if ($command -match '^\s*"(?<path>[^"]+\.exe)"(?:\s|$)') { return $Matches.path }
            return $null
        }
    }
    return $null
}

function Get-AlternateBackup([string] $currentBackup, [string] $protocol, [string] $commandKey, [string] $alternateBackupName) {
    if (-not (Test-Path -LiteralPath $currentBackup -PathType Leaf)) { return $null }
    $alternateOwnerFile = Join-Path $otherBackupDirectory "$protocol.owner"
    if (-not (Test-Path -LiteralPath $alternateOwnerFile -PathType Leaf)) { return $currentBackup }
    $alternateOwner = (Get-Content -LiteralPath $alternateOwnerFile -Raw -Encoding UTF8).Trim()
    $backupExecutable = Get-BackupExecutable $currentBackup $commandKey
    if (-not [string]::Equals($backupExecutable, $alternateOwner, [StringComparison]::OrdinalIgnoreCase)) {
        return $currentBackup
    }
    if (Test-Path -LiteralPath $alternateOwner -PathType Leaf) { return $currentBackup }

    $alternateBackup = Join-Path $otherBackupDirectory $alternateBackupName
    if (Test-Path -LiteralPath $alternateBackup -PathType Leaf) { return $alternateBackup }
    return $null
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
        $progIdBackup = Join-Path $backupDirectory "bplist-progid.reg"
        $otherBplistOwnerFile = Join-Path $otherBackupDirectory "bplist.owner"
        $restoreProgIdBackup = $progIdBackup
        $restoreExtensionBackup = Join-Path $backupDirectory "bplist-extension.reg"
        if ((Test-Path -LiteralPath $progIdBackup -PathType Leaf) -and (Test-Path -LiteralPath $otherBplistOwnerFile -PathType Leaf)) {
            $otherBplistOwner = (Get-Content -LiteralPath $otherBplistOwnerFile -Raw -Encoding UTF8).Trim()
            $previousExecutable = Get-BackupExecutable $progIdBackup "Software\Classes\BSManager.BeatSaberPlaylist\shell\open\command"
            $previousProfileOwnedBackup = [string]::Equals($previousExecutable, $otherBplistOwner, [StringComparison]::OrdinalIgnoreCase)
            $previousProfileMissing = -not (Test-Path -LiteralPath $otherBplistOwner -PathType Leaf)
            if ($previousProfileOwnedBackup -and $previousProfileMissing) {
                $restoreProgIdBackup = Join-Path $otherBackupDirectory "bplist-progid.reg"
                $restoreExtensionBackup = Join-Path $otherBackupDirectory "bplist-extension.reg"
            }
        }
        foreach ($registryBackup in @($restoreProgIdBackup, $restoreExtensionBackup)) {
            if (-not (Test-Path -LiteralPath $registryBackup -PathType Leaf)) { continue }
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
    $currentBackup = Join-Path $backupDirectory ($protocol + ".reg")
    $registryBackup = Get-AlternateBackup $currentBackup $protocol "Software\Classes\$protocol\shell\open\command" ($protocol + ".reg")
    if ($null -ne $registryBackup) {
        $restore = Start-Process -FilePath "$env:WINDIR\System32\reg.exe" `
            -ArgumentList @("import", "`"$registryBackup`"") -Wait -PassThru -NoNewWindow
        if ($restore.ExitCode -ne 0) {
            throw "Could not restore the previous $protocol registration. Backup: $registryBackup"
        }
    }
}
