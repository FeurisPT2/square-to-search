# Dot-source in PowerShell to add project-local tools and Cargo bin to PATH:
#   . .\scripts\use-tools.ps1

$repoRoot = Split-Path -Parent $PSScriptRoot
$toolsBin = Join-Path $repoRoot ".tools\bin"
$cargoBin = if ($env:CARGO_HOME) {
    Join-Path $env:CARGO_HOME "bin"
} else {
    Join-Path $env:USERPROFILE ".cargo\bin"
}

$currentEntries = $env:PATH -split ';' | Where-Object { $_ -ne "" }
$toPrepend = @()

if ((Test-Path $toolsBin) -and ($currentEntries -notcontains $toolsBin)) {
    $toPrepend += $toolsBin
}
if ((Test-Path $cargoBin) -and ($currentEntries -notcontains $cargoBin)) {
    $toPrepend += $cargoBin
}

if ($toPrepend.Count -gt 0) {
    $env:PATH = ($toPrepend -join ';') + ';' + $env:PATH
}
