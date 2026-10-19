# 使用工作区工具链；没有本地工具链时使用 PATH 中的 Cargo。
# 示例：.\scripts\cargo.ps1 test / .\scripts\cargo.ps1 build --release
# 测试参数的分隔符需加引号：.\scripts\cargo.ps1 test FILTER '--' --nocapture
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$localCargoHome = Join-Path $projectRoot '.toolchain/cargo'
$localRustupHome = Join-Path $projectRoot '.toolchain/rustup'
$cargoExecutable = Join-Path $localCargoHome 'bin/cargo.exe'
$savedCargoHome = $env:CARGO_HOME
$savedRustupHome = $env:RUSTUP_HOME
$savedPath = $env:PATH
$cargoExitCode = 1

try {
    if (Test-Path -LiteralPath $cargoExecutable) {
        $env:CARGO_HOME = $localCargoHome
        $env:RUSTUP_HOME = $localRustupHome
        $env:PATH = "$(Join-Path $localCargoHome 'bin');$savedPath"
    } else {
        $cargoExecutable = (Get-Command cargo -CommandType Application -ErrorAction Stop).Source
    }
    Push-Location -LiteralPath $projectRoot
    try {
        & $cargoExecutable @args
        $cargoExitCode = $LASTEXITCODE
    } finally {
        Pop-Location
    }
} finally {
    $env:CARGO_HOME = $savedCargoHome
    $env:RUSTUP_HOME = $savedRustupHome
    $env:PATH = $savedPath
}
exit $cargoExitCode
