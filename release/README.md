# Pātaka installers

Linux and Windows installers are built locally with the scripts in `../scripts/` and committed here. The macOS `.dmg` is built by the *macOS build* GitHub Actions workflow and attached to the [GitHub Release](https://github.com/woodRock/Pataka/releases/latest), together with these files.

| Folder | Contents | Built with |
|---|---|---|
| `linux/` | `.deb`, `.rpm`, `.AppImage` (x86_64) | `scripts/build-linux.sh` |
| `windows/` | NSIS `-setup.exe` installer (x86_64) | `scripts/build-windows.sh` (cross-compiled) or `scripts/build-windows.ps1` |
| `macos/` | universal `.dmg` (only when built locally on a Mac) | `scripts/build-macos.sh`, or the *macOS build* workflow |

Verify before installing:

```bash
sha256sum -c SHA256SUMS --ignore-missing     # Linux
shasum -a 256 -c SHA256SUMS --ignore-missing # macOS
```

On Windows: `Get-FileHash .\windows\<file>.exe -Algorithm SHA256`, then compare with the line in `SHA256SUMS`.

The installers are not code-signed. Windows SmartScreen will warn (More info → Run anyway). On macOS, an ad-hoc signed build from `build-macos.sh` runs on the Mac that built it; for other Macs, right-click → Open the first time, or sign and notarise with an Apple Developer ID.
