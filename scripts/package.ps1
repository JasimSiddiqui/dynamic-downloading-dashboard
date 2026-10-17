$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

npm run build
if ($LASTEXITCODE -ne 0) { throw "build failed" }

$version = (Get-Content src-tauri\tauri.conf.json -Raw | ConvertFrom-Json).version
$release = "src-tauri\target\release"
$out = "release-assets"
New-Item -ItemType Directory -Force $out | Out-Null

$setup = Get-ChildItem "$release\bundle\nsis\*_${version}_*-setup.exe" | Select-Object -First 1
Copy-Item $setup.FullName "$out\DynamicDownloadingDashboard-Setup.exe" -Force

$stage = Join-Path ([IO.Path]::GetTempPath()) "ddd-portable"
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null
Copy-Item "$release\dynamic-downloading-dashboard.exe" "$stage\DynamicDownloadingDashboard.exe"
Copy-Item LICENSE "$stage\LICENSE.txt"
Compress-Archive -Path "$stage\*" -DestinationPath "$out\DynamicDownloadingDashboard-Portable.zip" -CompressionLevel Optimal -Force
Remove-Item $stage -Recurse -Force

Get-ChildItem $out | ForEach-Object { "{0,-44} {1,8:N1} MB" -f $_.Name, ($_.Length / 1MB) }
