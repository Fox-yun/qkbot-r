param(
    [ValidateSet("core", "parser", "parser-full", "download", "web", "music", "full")]
    [string]$Profile = "core",
    [string]$Python = "python",
    [string]$EnvironmentPath = ""
)
$ErrorActionPreference = "Stop"
$repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
if (-not $EnvironmentPath) {
    $EnvironmentPath = Join-Path $repository "data/nonebot-venv"
}
$environment = [IO.Path]::GetFullPath($EnvironmentPath)
$executable = Join-Path $environment "Scripts/python.exe"
if (-not (Test-Path -LiteralPath $executable)) {
    & $Python -m venv $environment
    if ($LASTEXITCODE -ne 0) { throw "Failed to create the isolated Python environment." }
}
$lockName = if ($Profile -eq "core") { "requirements.lock" } else { "requirements-$Profile.lock" }
$lockPath = Join-Path $PSScriptRoot $lockName
& $executable -m pip install --require-hashes -r $lockPath
if ($LASTEXITCODE -ne 0) { throw "Dependency installation failed." }
Write-Output "Installed $Profile in $environment"
Write-Output "Browser binaries and ffmpeg are separate optional prerequisites; see README.md."
