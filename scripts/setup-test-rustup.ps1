[CmdletBinding()]
param(
    [string]$Toolchain = "1.95.0",
    [string]$TargetSubdir = "test-rustup",
    [switch]$Reset
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BaseDir = Join-Path $RepoRoot "target\$TargetSubdir"
$RustupHome = Join-Path $BaseDir "rustup"
$CargoHome = Join-Path $BaseDir "cargo"
$RustupInit = Join-Path $BaseDir "rustup-init.exe"

if ($Reset) {
    if (Test-Path $RustupHome) { Remove-Item -Recurse -Force $RustupHome }
    if (Test-Path $CargoHome) { Remove-Item -Recurse -Force $CargoHome }
}

New-Item -ItemType Directory -Path $BaseDir, $RustupHome, $CargoHome -Force | Out-Null

if (-not (Test-Path $RustupInit)) {
    Write-Host "Downloading rustup-init.exe..."
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $RustupInit
}

$env:RUSTUP_HOME = $RustupHome
$env:CARGO_HOME = $CargoHome

Write-Host "Initializing isolated rustup + cargo ($Toolchain, minimal profile) in $BaseDir..."
& $RustupInit -y --no-modify-path --default-toolchain $Toolchain --profile minimal
if ($LASTEXITCODE -ne 0) {
    throw "rustup-init.exe failed with exit code $LASTEXITCODE"
}

$CargoBin = Join-Path $CargoHome "bin"
$DefaultCargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
$FilteredPath = ($env:PATH -split ";") | Where-Object {
    $_ -and ($_.TrimEnd("\") -ine $DefaultCargoBin.TrimEnd("\")) -and ($_.TrimEnd("\") -ine $CargoBin.TrimEnd("\"))
}
$env:PATH = (@($CargoBin) + $FilteredPath) -join ";"

Write-Host ""
Write-Host "Isolated environment ready:"
& (Join-Path $CargoBin "rustup.exe") toolchain list
& (Join-Path $CargoBin "cargo.exe") --version

Write-Host ""
Write-Host "If you did not dot-source this script, activate it in your current PowerShell session with:"
Write-Host "  `$env:RUSTUP_HOME = '$RustupHome'"
Write-Host "  `$env:CARGO_HOME  = '$CargoHome'"
Write-Host "  `$env:PATH        = '$CargoBin;' + ((`$env:PATH -split ';' | Where-Object { `$_ -ne '$DefaultCargoBin' }) -join ';')"
