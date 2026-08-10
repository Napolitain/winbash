#Requires -Version 7.0

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9A-Za-z_.-]+/[0-9A-Za-z_.-]+$')]
    [string] $Repository,

    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string] $CommitSha,

    [Parameter(Mandatory = $true)]
    [string] $ReleaseVersion,

    [Parameter(Mandatory = $true)]
    [string] $ArchivePath,

    [Parameter(Mandatory = $true)]
    [string] $ChecksumPath,

    [Parameter(Mandatory = $true)]
    [string] $ManifestPath,

    [Parameter(Mandatory = $true)]
    [uri] $WorkflowUrl,

    [ValidatePattern('^[0-9A-Za-z_.-]+$')]
    [string] $ReleaseTag = 'continuous',

    [ValidateRange(1, 50)]
    [int] $RetainBuilds = 5
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    throw 'GitHub CLI (gh) is required to publish the release.'
}
if (-not $env:GH_TOKEN) {
    throw 'GH_TOKEN is required to publish the release.'
}

$ArchivePath = (Resolve-Path -LiteralPath $ArchivePath).Path
$ChecksumPath = (Resolve-Path -LiteralPath $ChecksumPath).Path
$ManifestPath = (Resolve-Path -LiteralPath $ManifestPath).Path

function Invoke-GitHubCli {
    param(
        [Parameter(Mandatory = $true)]
        [string[]] $Arguments,

        [switch] $AllowNotFound
    )

    $output = & gh @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    $message = ($output | Out-String).Trim()

    if ($exitCode -ne 0) {
        if ($AllowNotFound -and $message -match 'HTTP 404') {
            return $null
        }

        throw "gh $($Arguments -join ' ') failed with exit code ${exitCode}: $message"
    }

    return $message
}

function Get-ContinuousRelease {
    $json = Invoke-GitHubCli -Arguments @(
        'api',
        "repos/$Repository/releases/tags/$ReleaseTag"
    ) -AllowNotFound

    if (-not $json) {
        return $null
    }

    return $json | ConvertFrom-Json
}

function Test-CurrentMain {
    $mainReference = Invoke-GitHubCli -Arguments @(
        'api',
        "repos/$Repository/git/ref/heads/main",
        '--jq', '.object.sha'
    )

    if ($mainReference.Trim() -eq $CommitSha) {
        return $true
    }

    Write-Host "Skipping publication because $CommitSha is no longer the head of main ($mainReference)."
    return $false
}

function Set-ContinuousTag {
    $tagReference = Invoke-GitHubCli -Arguments @(
        'api',
        "repos/$Repository/git/ref/tags/$ReleaseTag"
    ) -AllowNotFound

    if ($tagReference) {
        $null = Invoke-GitHubCli -Arguments @(
            'api',
            '--method', 'PATCH',
            "repos/$Repository/git/refs/tags/$ReleaseTag",
            '-f', "sha=$CommitSha",
            '-F', 'force=true',
            '--silent'
        )
    } else {
        $null = Invoke-GitHubCli -Arguments @(
            'api',
            '--method', 'POST',
            "repos/$Repository/git/refs",
            '-f', "ref=refs/tags/$ReleaseTag",
            '-f', "sha=$CommitSha",
            '--silent'
        )
    }
}

function Get-ReleaseAssets {
    param(
        [Parameter(Mandatory = $true)]
        [long] $ReleaseId
    )

    $assetsJson = Invoke-GitHubCli -Arguments @(
        'api',
        '--paginate',
        '--slurp',
        "repos/$Repository/releases/$ReleaseId/assets?per_page=100"
    )
    $pages = $assetsJson | ConvertFrom-Json -NoEnumerate
    $assets = [Collections.Generic.List[object]]::new()

    foreach ($page in $pages) {
        foreach ($asset in $page) {
            $assets.Add($asset)
        }
    }

    return $assets.ToArray()
}

function Remove-ExpiredBuildAssets {
    $release = Get-ContinuousRelease
    if (-not $release) {
        return
    }

    $assets = @(Get-ReleaseAssets -ReleaseId $release.id)
    $archivePattern = '^winbash-.+-x86_64-pc-windows-msvc\.zip$'
    $packagePattern = '^winbash-.+-x86_64-pc-windows-msvc\.zip(?:\.sha256)?$'
    $assetNames = @($assets | ForEach-Object { $_.name })
    $completeArchives = @(
        $assets |
            Where-Object {
                $_.name -match $archivePattern -and
                $assetNames -contains "$($_.name).sha256"
            } |
            Sort-Object -Property @{ Expression = { [datetime] $_.created_at }; Descending = $true }
    )
    $retainedArchiveNames = @(
        $completeArchives |
            Select-Object -First $RetainBuilds |
            ForEach-Object { $_.name }
    )

    foreach ($asset in $assets) {
        if ($asset.name -notmatch $packagePattern) {
            continue
        }

        $archiveName = $asset.name -replace '\.sha256$', ''
        if ($retainedArchiveNames -contains $archiveName) {
            continue
        }

        $null = Invoke-GitHubCli -Arguments @(
            'api',
            '--method', 'DELETE',
            "repos/$Repository/releases/assets/$($asset.id)",
            '--silent'
        )
    }
}

if (-not (Test-CurrentMain)) {
    return
}

$shortCommit = $CommitSha.Substring(0, 7).ToLowerInvariant()
$title = "winbash continuous ($ReleaseVersion)"
$notesPath = Join-Path ([IO.Path]::GetTempPath()) "winbash-release-notes-$([guid]::NewGuid()).md"
$notes = @"
This mutable prerelease tracks the newest successful build from ``main``.

- Version: ``$ReleaseVersion``
- Commit: [$shortCommit](https://github.com/$Repository/commit/$CommitSha)
- Workflow: [GitHub Actions run]($WorkflowUrl)

Install or update through the attached ``winbash.json`` Scoop manifest.
"@

try {
    [IO.File]::WriteAllText($notesPath, "$notes`n", [Text.UTF8Encoding]::new($false))
    $release = Get-ContinuousRelease

    if ($release -and -not $release.draft -and $release.prerelease -and $release.name -eq $title) {
        $assetNames = @(
            Get-ReleaseAssets -ReleaseId $release.id |
                ForEach-Object { $_.name }
        )
        $expectedAssetNames = @(
            [IO.Path]::GetFileName($ArchivePath),
            [IO.Path]::GetFileName($ChecksumPath),
            [IO.Path]::GetFileName($ManifestPath)
        )
        $missingAssetNames = @($expectedAssetNames | Where-Object { $assetNames -notcontains $_ })

        if ($missingAssetNames.Count -eq 0) {
            if (-not (Test-CurrentMain)) {
                return
            }

            Set-ContinuousTag
            Remove-ExpiredBuildAssets
            Write-Host "$ReleaseVersion is already published to the $ReleaseTag prerelease."
            return
        }
    }

    if (-not $release) {
        Set-ContinuousTag
        $null = Invoke-GitHubCli -Arguments @(
            'release', 'create', $ReleaseTag,
            '--repo', $Repository,
            '--draft',
            '--prerelease',
            '--verify-tag',
            '--title', $title,
            '--notes-file', $notesPath
        )
    }

    $null = Invoke-GitHubCli -Arguments @(
        'release', 'upload', $ReleaseTag,
        $ArchivePath, $ChecksumPath,
        '--repo', $Repository,
        '--clobber'
    )
    if (-not (Test-CurrentMain)) {
        return
    }

    Set-ContinuousTag
    $null = Invoke-GitHubCli -Arguments @(
        'release', 'upload', $ReleaseTag,
        $ManifestPath,
        '--repo', $Repository,
        '--clobber'
    )
    $null = Invoke-GitHubCli -Arguments @(
        'release', 'edit', $ReleaseTag,
        '--repo', $Repository,
        '--draft=false',
        '--prerelease',
        '--title', $title,
        '--notes-file', $notesPath
    )
    Remove-ExpiredBuildAssets
    Write-Host "Published $ReleaseVersion to the $ReleaseTag prerelease."
} finally {
    if (Test-Path -LiteralPath $notesPath) {
        Remove-Item -LiteralPath $notesPath -Force
    }
}
