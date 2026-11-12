# Install the `pataka` command on Windows: Claude Code with Pātaka's
# Tūmataiti redaction in the middle. Usage (PowerShell):
#
#   irm https://raw.githubusercontent.com/woodRock/Pataka/main/install.ps1 | iex
#
# Downloads the prebuilt binary from the repository's release\ folder, checks
# it against release\SHA256SUMS, puts it in %LOCALAPPDATA%\Pataka\bin (or
# $env:PATAKA_INSTALL_DIR) and adds that folder to your user PATH. Run
# `pataka setup` afterwards.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Repo = if ($env:PATAKA_REPO) { $env:PATAKA_REPO } else { 'woodRock/Pataka' }
$Base = if ($env:PATAKA_BASE) { $env:PATAKA_BASE } else { "https://github.com/$Repo/raw/main/release" }
$Dest = if ($env:PATAKA_INSTALL_DIR) { $env:PATAKA_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Pataka\bin' }

if (-not [Environment]::Is64BitOperatingSystem) { throw 'pataka needs 64-bit Windows.' }
$target = 'x86_64-pc-windows-gnu'

$sums = (Invoke-WebRequest "$Base/SHA256SUMS" -UseBasicParsing).Content
$line = $sums -split "`n" | Where-Object { $_ -match " cli/pataka-[0-9.]+-$target\.exe\s*$" } | Select-Object -Last 1
if (-not $line) { throw "No pataka build for $target is published yet." }
$expected, $file = $line.Trim() -split '\s+', 2
$version = ($file -replace '^cli/pataka-([0-9.]+)-.*$', '$1')

Write-Host "Downloading pataka $version for Windows..."
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("pataka-" + [guid]::NewGuid().ToString() + ".exe")
Invoke-WebRequest "$Base/$file" -OutFile $tmp -UseBasicParsing
$actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected.ToLower()) {
  Remove-Item $tmp -Force
  throw 'The download does not match its SHA-256, so it was not installed.'
}

New-Item -ItemType Directory -Force $Dest | Out-Null
Move-Item -Force $tmp (Join-Path $Dest 'pataka.exe')
Write-Host "Installed $Dest\pataka.exe (SHA-256 checked)."

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $userPath) { $userPath = '' }
if (($userPath -split ';') -notcontains $Dest) {
  [Environment]::SetEnvironmentVariable('Path', ($userPath.TrimEnd(';') + ';' + $Dest).TrimStart(';'), 'User')
  Write-Host "Added $Dest to your PATH. Open a new terminal to use it."
}
Write-Host ''
Write-Host 'Next: pataka setup'
