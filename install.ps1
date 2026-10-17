# local-brain installer for Windows.
#   irm https://raw.githubusercontent.com/koztay/local-brain/main/install.ps1 | iex
# Environment: LOCAL_BRAIN_VERSION (default: latest), LOCAL_BRAIN_INSTALL_DIR
$ErrorActionPreference = "Stop"
$repo = "koztay/local-brain"
$version = if ($env:LOCAL_BRAIN_VERSION) { $env:LOCAL_BRAIN_VERSION } else { "latest" }
$dir = if ($env:LOCAL_BRAIN_INSTALL_DIR) { $env:LOCAL_BRAIN_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "local-brain\bin" }
$asset = "local-brain-x86_64-pc-windows-msvc.zip"
$base = if ($version -eq "latest") { "https://github.com/$repo/releases/latest/download" } else { "https://github.com/$repo/releases/download/$version" }

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("local-brain-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
Write-Host "local-brain: downloading $base/$asset"
Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $tmp $asset)
Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp "SHA256SUMS")
$expected = (Get-Content (Join-Path $tmp "SHA256SUMS") | Where-Object { $_ -match [regex]::Escape($asset) }) -split '\s+' | Select-Object -First 1
$actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash.ToLower()
if ($expected -and $expected -ne $actual) { throw "checksum mismatch for $asset" }
Write-Host "local-brain: checksum verified"

Expand-Archive (Join-Path $tmp $asset) -DestinationPath $tmp -Force
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item (Join-Path $tmp "local-brain.exe") (Join-Path $dir "local-brain.exe") -Force
Remove-Item $tmp -Recurse -Force

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($userPath -split ';' | Where-Object { $_ -eq $dir })) {
  [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
  Write-Host "local-brain: added $dir to your PATH (open a new terminal)"
}
Write-Host "local-brain: installed $(Join-Path $dir 'local-brain.exe')"
Write-Host "Next: cd your-project; local-brain init; local-brain learn"
