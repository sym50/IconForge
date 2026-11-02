param(
    [string]$Cargo = 'cargo',
    [string]$Rustc = 'rustc',
    [switch]$Test,
    [switch]$Offline
)
$ErrorActionPreference = 'Stop'
$projectRoot = $PSScriptRoot
$versionInfo = & $Rustc -vV
if ($LASTEXITCODE -ne 0 -or !($versionInfo -match 'host: x86_64-pc-windows-gnu')) {
    throw 'Use the x86_64-pc-windows-gnu Rust toolchain (see BUILDING.md).'
}
$toolchainRoot = (& $Rustc --print sysroot).Trim()
$tools = Join-Path $toolchainRoot 'lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained'
$windres = Join-Path $tools 'windres.exe'
if (!(Test-Path -LiteralPath $windres)) {
    $found = Get-Command windres.exe -ErrorAction SilentlyContinue
    if (!$found) { throw 'GNU windres.exe was not found.' }
    $windres = $found.Source
}
$env:PATH = "$(Join-Path $toolchainRoot 'bin');${tools};$env:PATH"
$env:RUSTC = $Rustc
Push-Location (Join-Path $projectRoot 'assets')
try {
    & $windres --use-temp-file '--preprocessor=cmd.exe' '--preprocessor-arg=/c' '--preprocessor-arg=type' -i app.rc -o app_res.o
    if ($LASTEXITCODE -ne 0) { throw 'Resource compilation failed.' }
} finally { Pop-Location }
$cargoArgs = @('--manifest-path', (Join-Path $projectRoot 'Cargo.toml'), '--release', '--locked')
if ($Offline) { $cargoArgs += '--offline' }
if ($Test) {
    & $Cargo test @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw 'Tests failed.' }
}
& $Cargo build @cargoArgs
if ($LASTEXITCODE -ne 0) { throw 'Build failed.' }
Write-Output 'Build completed. See target/release/iconforge.exe (or CARGO_TARGET_DIR).'
