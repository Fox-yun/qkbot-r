param(
    [string[]]$Profile = @(),
    [string]$EnvironmentPath = "",
    [string]$Url = "http://127.0.0.1:3215",
    [string]$PluginConfig = ""
)
$ErrorActionPreference = "Stop"
$repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "../.."))
if (-not $EnvironmentPath) { $EnvironmentPath = Join-Path $repository "data/nonebot-venv" }
$executable = Join-Path ([IO.Path]::GetFullPath($EnvironmentPath)) "Scripts/python.exe"
if (-not (Test-Path -LiteralPath $executable)) { throw "Run install.ps1 first." }
if (-not $env:QKBOT_NONEBOT_TOKEN) {
    throw "Set QKBOT_NONEBOT_TOKEN to the separate Rust bridge token."
}
$arguments = @((Join-Path $PSScriptRoot "run.py"), "--url", $Url)
foreach ($item in $Profile) { $arguments += @("--profile", $item) }
if ($PluginConfig) { $arguments += @("--plugin-config", ([IO.Path]::GetFullPath($PluginConfig))) }
& $executable @arguments
exit $LASTEXITCODE
