<#
.SYNOPSIS
    Stages and packages the portable Windows x64 release archive for square-to-search.

.DESCRIPTION
    Builds `sts-app` in release mode (unless -SkipBuild is passed), stages `sts-app.exe`,
    `README.md`, and dereferenced runtime DLLs (notably resolving the `DirectML.dll` symlink),
    enforces the Decision 3A zero-CUDA DLL check, and writes `square-to-search-windows-x64.zip`
    plus its `.sha256` checksum file.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [string]$OutDir = "dist"
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path -LiteralPath (Split-Path -Parent $scriptDir)).Path

# Ensure Cargo toolchain binaries are available on PATH.
$cargoBin = if ($env:CARGO_HOME) {
    Join-Path $env:CARGO_HOME "bin"
} else {
    Join-Path $env:USERPROFILE ".cargo\bin"
}
$pathEntries = @($env:PATH -split ';' | Where-Object { $_ -ne "" })
if ((Test-Path -LiteralPath $cargoBin) -and ($pathEntries -notcontains $cargoBin)) {
    $env:PATH = "$cargoBin;$env:PATH"
}

$resolvedOutDir = if ([System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir
} else {
    Join-Path $repoRoot $OutDir
}

$releaseDir = Join-Path $repoRoot "target\release"
$packageName = "square-to-search-windows-x64"
$stagingRoot = Join-Path $resolvedOutDir "staging"
$stageDir = Join-Path $stagingRoot $packageName
$zipFileName = "$packageName.zip"
$zipPath = Join-Path $resolvedOutDir $zipFileName
$shaPath = "$zipPath.sha256"

# Decision 3A: never bundle CUDA/cuDNN/cuBLAS/cuFFT/cuRAND/cuSOLVER/cuSPARSE/NCCL/NVRTC DLLs.
$cudaDllPattern = '(cuda|cudnn|cublas|cufft|curand|cusolver|cusparse|nccl|nvrtc)'

function Assert-NoCudaDlls {
    param(
        [Parameter(Mandatory = $true)][string]$Directory,
        [Parameter(Mandatory = $true)][string]$StageLabel
    )

    if (-not (Test-Path -LiteralPath $Directory)) {
        return
    }

    $dlls = @(Get-ChildItem -LiteralPath $Directory -Filter "*.dll" -Force | Where-Object { -not $_.PSIsContainer })
    foreach ($dll in $dlls) {
        if ($dll.Name -imatch $cudaDllPattern) {
            throw "Decision 3A violation in ${StageLabel}: forbidden CUDA runtime DLL detected: $($dll.FullName)"
        }
    }
}

function Resolve-FileSymlinkTarget {
    param(
        [Parameter(Mandatory = $true)][System.IO.FileSystemInfo]$Item
    )

    $current = $Item
    for ($depth = 0; $depth -lt 16; $depth++) {
        $isReparse = ($current.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0
        $linkTypeProp = $current.PSObject.Properties["LinkType"]
        $isSymlink = $linkTypeProp -and ($linkTypeProp.Value -eq "SymbolicLink")

        if (-not ($isReparse -or $isSymlink)) {
            return $current.FullName
        }

        $targetValue = $null
        $targetProp = $current.PSObject.Properties["Target"]
        if ($targetProp -and $targetProp.Value) {
            if ($targetProp.Value -is [System.Array]) {
                $targetValue = [string]$targetProp.Value[0]
            } else {
                $targetValue = [string]$targetProp.Value
            }
        }

        if ([string]::IsNullOrWhiteSpace($targetValue)) {
            # Fallback to .NET ResolveLinkTarget when available (PowerShell 7+) or return path for File.Copy.
            $resolveMethod = $current.PSObject.Methods["ResolveLinkTarget"]
            if ($resolveMethod) {
                $resolved = $current.ResolveLinkTarget($true)
                if ($resolved) {
                    return $resolved.FullName
                }
            }
            return $current.FullName
        }

        $candidatePath = if ([System.IO.Path]::IsPathRooted($targetValue)) {
            $targetValue
        } else {
            Join-Path (Split-Path -Parent $current.FullName) $targetValue
        }

        if (-not (Test-Path -LiteralPath $candidatePath)) {
            throw "Broken symlink for '$($Item.FullName)': resolved target '$candidatePath' does not exist."
        }

        $current = Get-Item -LiteralPath $candidatePath -Force
    }

    throw "Exceeded maximum symlink resolution depth for '$($Item.FullName)'."
}

function Copy-DereferencedFile {
    param(
        [Parameter(Mandatory = $true)][string]$SourcePath,
        [Parameter(Mandatory = $true)][string]$DestinationPath
    )

    $item = Get-Item -LiteralPath $SourcePath -Force
    $resolvedSource = Resolve-FileSymlinkTarget -Item $item

    if (-not (Test-Path -LiteralPath $resolvedSource)) {
        throw "Resolved source file does not exist: '$resolvedSource' (from '$SourcePath')."
    }

    # [System.IO.File]::Copy writes a real regular file (never a symlink/reparse point).
    [System.IO.File]::Copy($resolvedSource, $DestinationPath, $true)

    $staged = Get-Item -LiteralPath $DestinationPath -Force
    if (($staged.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
        throw "Staged file '$DestinationPath' is still a reparse point/symlink."
    }
    if ($staged.Length -le 0) {
        throw "Staged file '$DestinationPath' is empty (0 bytes)."
    }
}

Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        Write-Host "[package-windows] Building release binary: cargo build --release -p sts-app"
        & cargo build --release -p sts-app
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release -p sts-app failed with exit code $LASTEXITCODE"
        }
    }

    $exePath = Join-Path $releaseDir "sts-app.exe"
    if (-not (Test-Path -LiteralPath $exePath)) {
        throw "Release binary not found: '$exePath'. Run without -SkipBuild or build sts-app first."
    }
    $exeInfo = Get-Item -LiteralPath $exePath -Force
    if ($exeInfo.Length -le 0) {
        throw "Release binary '$exePath' is empty (0 bytes)."
    }

    $readmePath = Join-Path $repoRoot "README.md"
    if (-not (Test-Path -LiteralPath $readmePath)) {
        throw "README.md not found at repository root: '$readmePath'."
    }

    $directMlSrc = Join-Path $releaseDir "DirectML.dll"
    if (-not (Test-Path -LiteralPath $directMlSrc)) {
        throw "Required runtime DLL not found in release output: '$directMlSrc'."
    }

    # Decision 3A guard on target\release\*.dll before staging.
    Assert-NoCudaDlls -Directory $releaseDir -StageLabel "target\release"

    if (Test-Path -LiteralPath $stageDir) {
        Remove-Item -LiteralPath $stageDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
    New-Item -ItemType Directory -Path $resolvedOutDir -Force | Out-Null

    Copy-DereferencedFile -SourcePath $exePath -DestinationPath (Join-Path $stageDir "sts-app.exe")
    Copy-DereferencedFile -SourcePath $readmePath -DestinationPath (Join-Path $stageDir "README.md")

    # Stage DirectML.dll and all other non-CUDA runtime DLLs next to sts-app.exe, resolving symlinks.
    $runtimeDlls = @(Get-ChildItem -LiteralPath $releaseDir -Filter "*.dll" -Force | Where-Object { -not $_.PSIsContainer })
    foreach ($dll in $runtimeDlls) {
        $destDll = Join-Path $stageDir $dll.Name
        Copy-DereferencedFile -SourcePath $dll.FullName -DestinationPath $destDll
    }

    $stagedDirectMl = Join-Path $stageDir "DirectML.dll"
    if (-not (Test-Path -LiteralPath $stagedDirectMl)) {
        throw "Staged DirectML.dll is missing at '$stagedDirectMl'."
    }
    $stagedDirectMlInfo = Get-Item -LiteralPath $stagedDirectMl -Force
    if ($stagedDirectMlInfo.Length -le 0) {
        throw "Staged DirectML.dll at '$stagedDirectMl' is 0 bytes!"
    }

    # Decision 3A guard on staged *.dll.
    Assert-NoCudaDlls -Directory $stageDir -StageLabel "staging"

    if (Test-Path -LiteralPath $zipPath) {
        Remove-Item -LiteralPath $zipPath -Force
    }

    Write-Host "[package-windows] Creating archive '$zipPath'..."
    $stageItems = @(Get-ChildItem -LiteralPath $stageDir -Force | ForEach-Object { $_.FullName })
    Compress-Archive -LiteralPath $stageItems -DestinationPath $zipPath -CompressionLevel Optimal -Force

    if (-not (Test-Path -LiteralPath $zipPath)) {
        throw "Failed to create zip archive '$zipPath'."
    }

    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $shaLine = "$hash  $zipFileName`n"
    [System.IO.File]::WriteAllText($shaPath, $shaLine, [System.Text.UTF8Encoding]::new($false))

    $zipInfo = Get-Item -LiteralPath $zipPath -Force
    Write-Host "[package-windows] Done: '$zipPath' ($($zipInfo.Length) bytes)"
    Write-Host "[package-windows] DirectML.dll staged: $($stagedDirectMlInfo.Length) bytes"
    Write-Host "[package-windows] SHA-256 ($shaPath): $hash"
}
finally {
    Pop-Location
}
