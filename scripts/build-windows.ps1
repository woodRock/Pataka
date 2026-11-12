# Build Pātaka on Windows: NSIS installer and MSI into release\windows.
# Needs: Rust (https://rustup.rs, MSVC toolchain), Node.js 20+, and the
# "Desktop development with C++" workload from Visual Studio Build Tools.
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root
npm ci --no-audit --no-fund
cargo build --release --locked -p pataka-cli
New-Item -ItemType Directory -Force "src-tauri\binaries" | Out-Null
Copy-Item "target\release\pataka.exe" "src-tauri\binaries\pataka-cli-x86_64-pc-windows-msvc.exe"
npx tauri build --bundles nsis,msi
$out = Join-Path $root "release\windows"
if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $out | Out-Null
Copy-Item "target\release\bundle\nsis\*.exe" $out
Copy-Item "target\release\bundle\msi\*.msi" $out
Get-ChildItem $out | ForEach-Object { "{0}  windows/{1}" -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name } | Out-File -Encoding ascii (Join-Path $out "SHA256SUMS.windows")
Write-Host "done: $out"
