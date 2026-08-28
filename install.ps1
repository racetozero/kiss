[CmdletBinding()]
param(
    [string]$Version = $env:KISS_VERSION,
    [string]$Target = $env:KISS_TARGET,
    [string]$InstallDirectory = $env:KISS_INSTALL_DIR,
    [string]$Repository = $env:KISS_REPOSITORY,
    [string]$ReleasesUrl = $env:KISS_RELEASES_URL
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 3.0

function Fail([string]$Message) {
    throw "kiss installer: $Message"
}

function Normalize-Tag([string]$Requested) {
    $Normalized = if ($Requested.StartsWith("v")) { $Requested } else { "v$Requested" }
    $PlainVersion = $Normalized.Substring(1)
    if (-not $PlainVersion -or $PlainVersion -notmatch '^[0-9A-Za-z.+-]+$') {
        Fail "invalid version: $Requested"
    }
    return @($Normalized, $PlainVersion)
}

function Test-GitHubCli {
    if ($ReleasesUrl -or -not (Get-Command gh -ErrorAction SilentlyContinue)) {
        return $false
    }
    & gh auth status --hostname github.com *> $null
    return $LASTEXITCODE -eq 0
}

function Receive-File([string]$Source, [string]$Destination) {
    $Uri = [Uri]$Source
    if ($Uri.Scheme -eq "file") {
        Copy-Item -LiteralPath $Uri.LocalPath -Destination $Destination
        return
    }
    if ($Uri.Scheme -ne "https") {
        Fail "release downloads must use https or file URLs"
    }
    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Destination
}

try {
    if (-not $Repository) {
        $Repository = "racetozero/kiss"
    }
    $UseGitHubCli = Test-GitHubCli
    if (-not $ReleasesUrl) {
        $ReleasesUrl = "https://github.com/$Repository/releases"
    }

    if (-not $Version -or $Version -eq "latest") {
        if ($UseGitHubCli) {
            $Version = (& gh release view --repo $Repository --json tagName --jq .tagName).Trim()
            if ($LASTEXITCODE -ne 0 -or -not $Version) {
                Fail "could not find the latest release with gh"
            }
        } elseif ($env:KISS_RELEASES_URL) {
            Fail "set KISS_VERSION when KISS_RELEASES_URL is set"
        } else {
            try {
                $Release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repository/releases/latest"
                $Version = $Release.tag_name
            } catch {
                Fail "could not find the latest release; authenticate gh for a private repository"
            }
        }
    }
    $Tag, $PlainVersion = Normalize-Tag $Version

    if (-not $Target) {
        $Architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
        if ($Architecture -ne "X64") {
            Fail "unsupported Windows processor: $Architecture"
        }
        $Target = "x86_64-pc-windows-msvc"
    }
    if ($Target -ne "x86_64-pc-windows-msvc") {
        Fail "unsupported target: $Target"
    }

    if (-not $InstallDirectory) {
        if (-not $HOME) {
            Fail "HOME is not set; set KISS_INSTALL_DIR"
        }
        $InstallDirectory = Join-Path $HOME ".local\bin"
    }

    $ArchiveName = "kiss-$Target.zip"
    $ChecksumName = "$ArchiveName.sha256"
    $TemporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("kiss-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $TemporaryDirectory | Out-Null

    try {
        Write-Host "Installing kiss $PlainVersion for $Target"
        if ($UseGitHubCli) {
            & gh release download $Tag --repo $Repository --pattern $ArchiveName --pattern $ChecksumName --dir $TemporaryDirectory --clobber
            if ($LASTEXITCODE -ne 0) {
                Fail "could not download $Tag with gh"
            }
        } else {
            $AssetBase = "$($ReleasesUrl.TrimEnd('/'))/download/$Tag"
            Receive-File "$AssetBase/$ArchiveName" (Join-Path $TemporaryDirectory $ArchiveName)
            Receive-File "$AssetBase/$ChecksumName" (Join-Path $TemporaryDirectory $ChecksumName)
        }

        $ArchivePath = Join-Path $TemporaryDirectory $ArchiveName
        $ChecksumPath = Join-Path $TemporaryDirectory $ChecksumName
        $Expected = ((Get-Content -LiteralPath $ChecksumPath | Where-Object { $_.Trim() } | Select-Object -First 1) -split '\s+')[0]
        if ($Expected -notmatch '^[0-9A-Fa-f]{64}$') {
            Fail "the checksum file is invalid"
        }
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash
        if ($Actual -ne $Expected) {
            Fail "checksum verification failed for $ArchiveName"
        }

        $ExtractDirectory = Join-Path $TemporaryDirectory "extract"
        Expand-Archive -LiteralPath $ArchivePath -DestinationPath $ExtractDirectory
        $Binary = Join-Path $ExtractDirectory "kiss-$Target\kiss.exe"
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
            $Binary = Join-Path $ExtractDirectory "kiss.exe"
        }
        if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
            Fail "the release archive does not contain kiss.exe"
        }

        New-Item -ItemType Directory -Force -Path $InstallDirectory | Out-Null
        $Destination = Join-Path $InstallDirectory "kiss.exe"
        $Staged = Join-Path $InstallDirectory (".kiss.install." + [Guid]::NewGuid() + ".exe")
        Copy-Item -LiteralPath $Binary -Destination $Staged
        Move-Item -Force -LiteralPath $Staged -Destination $Destination
        Write-Host "Installed kiss $PlainVersion to $Destination"

        $PathEntries = $env:PATH -split ';'
        if ($InstallDirectory -notin $PathEntries) {
            Write-Host "Add $InstallDirectory to PATH to run kiss from any directory."
        }
    } finally {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TemporaryDirectory
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
