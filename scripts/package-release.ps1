[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?$')]
    [string] $BaseVersion,

    [Parameter(Mandatory = $true)]
    [ValidateRange(1, [long]::MaxValue)]
    [long] $RunNumber,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{7,40}$')]
    [string] $CommitSha,

    [Parameter(Mandatory = $true)]
    [string] $BinaryPath,

    [Parameter(Mandatory = $true)]
    [string] $OutputDirectory,

    [ValidatePattern('^[0-9A-Za-z_.-]+/[0-9A-Za-z_.-]+$')]
    [string] $Repository = 'Napolitain/winbash',

    [ValidatePattern('^[0-9A-Za-z_.-]+$')]
    [string] $ReleaseTag = 'continuous'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$resolvedBinary = (Resolve-Path -LiteralPath $BinaryPath).Path
$readmePath = Join-Path $repositoryRoot 'README.md'
$licensePath = Join-Path $repositoryRoot 'LICENSE'

foreach ($requiredPath in @($readmePath, $licensePath)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required package file does not exist: $requiredPath"
    }
}

$null = New-Item -ItemType Directory -Force -Path $OutputDirectory
$resolvedOutput = (Resolve-Path -LiteralPath $OutputDirectory).Path
$shortCommit = $CommitSha.Substring(0, 7).ToLowerInvariant()
$releaseVersion = "$BaseVersion-main.$RunNumber.g$shortCommit"
$archiveName = "winbash-$releaseVersion-x86_64-pc-windows-msvc.zip"
$archivePath = Join-Path $resolvedOutput $archiveName
$checksumPath = "$archivePath.sha256"
$manifestPath = Join-Path $resolvedOutput 'winbash.json'
$stagingDirectory = Join-Path ([IO.Path]::GetTempPath()) "winbash-package-$([guid]::NewGuid())"

try {
    $null = New-Item -ItemType Directory -Path $stagingDirectory
    Copy-Item -LiteralPath $resolvedBinary -Destination (Join-Path $stagingDirectory 'winbash.exe')
    Copy-Item -LiteralPath $readmePath -Destination (Join-Path $stagingDirectory 'README.md')
    Copy-Item -LiteralPath $licensePath -Destination (Join-Path $stagingDirectory 'LICENSE')

    Compress-Archive -Path (Join-Path $stagingDirectory '*') -DestinationPath $archivePath -CompressionLevel Optimal -Force
} finally {
    if (Test-Path -LiteralPath $stagingDirectory) {
        Remove-Item -LiteralPath $stagingDirectory -Recurse -Force
    }
}

$archiveHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
$utf8WithoutBom = [Text.UTF8Encoding]::new($false)
[IO.File]::WriteAllText($checksumPath, "$archiveHash  $archiveName`n", $utf8WithoutBom)

$archiveUrl = "https://github.com/$Repository/releases/download/$ReleaseTag/$archiveName"
$manifest = [ordered]@{
    version = $releaseVersion
    description = 'A deliberately small zsh-like shell for Windows'
    homepage = "https://github.com/$Repository"
    license = 'MIT'
    depends = 'main/uutils-coreutils'
    architecture = [ordered]@{
        '64bit' = [ordered]@{
            url = $archiveUrl
            hash = $archiveHash
        }
    }
    bin = 'winbash.exe'
}

$manifestJson = $manifest | ConvertTo-Json -Depth 5
[IO.File]::WriteAllText($manifestPath, "$manifestJson`n", $utf8WithoutBom)

[pscustomobject]@{
    ReleaseVersion = $releaseVersion
    ArchiveName = $archiveName
    ArchivePath = $archivePath
    ChecksumPath = $checksumPath
    ManifestPath = $manifestPath
    ArchiveHash = $archiveHash
}
