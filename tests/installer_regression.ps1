$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

$RepositoryRoot = Split-Path -Parent $PSScriptRoot
$Installer = Join-Path $RepositoryRoot "install.ps1"
$TemporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("kiss-installer-test-" + [Guid]::NewGuid())
$Target = "x86_64-pc-windows-msvc"
$Tag = "v0.0.1"
$ArchiveName = "kiss-$Target.zip"
$ReleaseDirectory = Join-Path $TemporaryRoot "releases\download\$Tag"
$PayloadRoot = Join-Path $TemporaryRoot "payload"
$PayloadDirectory = Join-Path $PayloadRoot "kiss-$Target"
$InstallDirectory = Join-Path $TemporaryRoot "install"
$ReleasesUrl = "file:///" + ((Join-Path $TemporaryRoot "releases") -replace '\\', '/')

function Write-Archive([string]$Payload) {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $PayloadRoot
    New-Item -ItemType Directory -Force -Path $PayloadDirectory | Out-Null
    [IO.File]::WriteAllText((Join-Path $PayloadDirectory "kiss.exe"), $Payload)
    $ArchivePath = Join-Path $ReleaseDirectory $ArchiveName
    Remove-Item -Force -ErrorAction SilentlyContinue $ArchivePath
    Compress-Archive -Path $PayloadDirectory -DestinationPath $ArchivePath
    $Hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash
    [IO.File]::WriteAllText("$ArchivePath.sha256", "$Hash  $ArchiveName`n")
}

function Invoke-Installer {
    $InstallerArguments = @{
        Version = "0.0.1"
        Target = $Target
        InstallDirectory = $InstallDirectory
        ReleasesUrl = $ReleasesUrl
    }
    & $Installer @InstallerArguments
}

try {
    New-Item -ItemType Directory -Force -Path $ReleaseDirectory, $InstallDirectory | Out-Null

    Write-Archive "first"
    Invoke-Installer
    if ([IO.File]::ReadAllText((Join-Path $InstallDirectory "kiss.exe")) -ne "first") {
        throw "PowerShell installer did not install the expected payload"
    }
    Write-Host "ok: PowerShell installer installs a verified archive"

    Write-Archive "replacement"
    Invoke-Installer
    if ([IO.File]::ReadAllText((Join-Path $InstallDirectory "kiss.exe")) -ne "replacement") {
        throw "PowerShell installer did not replace the payload"
    }
    Write-Host "ok: PowerShell installer replaces an existing binary"

    [IO.File]::WriteAllText((Join-Path $ReleaseDirectory "$ArchiveName.sha256"), ("0" * 64) + "  $ArchiveName`n")
    $PowerShellExecutable = (Get-Process -Id $PID).Path
    $FailureArguments = @(
        "-NoProfile", "-File", $Installer,
        "-Version", "0.0.1",
        "-Target", $Target,
        "-InstallDirectory", $InstallDirectory,
        "-ReleasesUrl", $ReleasesUrl
    )
    & $PowerShellExecutable @FailureArguments *> $null
    if ($LASTEXITCODE -eq 0) {
        throw "checksum failure unexpectedly succeeded"
    }
    if ([IO.File]::ReadAllText((Join-Path $InstallDirectory "kiss.exe")) -ne "replacement") {
        throw "checksum failure replaced the existing payload"
    }
    Write-Host "ok: checksum failure keeps the existing Windows binary"

    $FailureArguments = @(
        "-NoProfile", "-File", $Installer,
        "-Version", "0.0.1",
        "-Target", "unsupported-target",
        "-InstallDirectory", $InstallDirectory,
        "-ReleasesUrl", $ReleasesUrl
    )
    & $PowerShellExecutable @FailureArguments *> $null
    if ($LASTEXITCODE -eq 0) {
        throw "unsupported PowerShell target unexpectedly succeeded"
    }
    Write-Host "ok: unsupported PowerShell target fails"
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TemporaryRoot
}
