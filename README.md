<div align="center">
  <img src="assets/logo.png" alt="Duckling Browser Logo" width="150">
  <h1>Duckling Browser</h1>
  <strong>Open Source Anti-Detect Browser — All Features Free &amp; Unlocked</strong>
  <br>
  <a href="https://github.com/TomiWebPro/ducklingbrowser">github.com/TomiWebPro/ducklingbrowser</a>
</div>

> **Fork notice**: This is a community fork of the original Donut Browser (AGPL-3.0). All paid/pro subscription gating has been removed — every feature is free and unlocked by default with no gatekeeping, paywalls, or plan restrictions. Contributions and issue reports welcome at [TomiWebPro/ducklingbrowser](https://github.com/TomiWebPro/ducklingbrowser).
<br>

<p align="center">
  <a style="text-decoration: none;" href="https://github.com/TomiWebPro/ducklingbrowser/releases/latest" target="_blank"><img alt="GitHub release" src="https://img.shields.io/github/v/release/TomiWebPro/ducklingbrowser">
  </a>
  <a style="text-decoration: none;" href="https://github.com/TomiWebPro/ducklingbrowser/issues" target="_blank">
    <img src="https://img.shields.io/badge/PRs-welcome-brightgreen.svg?style=flat" alt="PRs Welcome">
  </a>
  <a style="text-decoration: none;" href="https://github.com/TomiWebPro/ducklingbrowser/blob/main/LICENSE" target="_blank">
    <img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License">
  </a>
  <a style="text-decoration: none;" href="https://github.com/TomiWebPro/ducklingbrowser/network/members" target="_blank">
    <img src="https://img.shields.io/github/forks/TomiWebPro/ducklingbrowser?style=social" alt="GitHub forks">
  </a>
</p>

<img alt="Duckling Browser Preview" src="assets/duckling-preview.png" />

## Features

- **Unlimited browser profiles**: each fully isolated with its own fingerprint, cookies, extensions, and data
- **Anti-detect Chromium engine**: powered by [Wayfern](https://wayfern.com), which is privacy-focused Chromium fork that comes with advanced fingerprint spoofing which naturally hides information in a way that is not detected by Cloudflare, reCaptcha v3, and other browser fingerprinting and anti-bot services.
- **DNS AdBlocker** - block ads, trackers, and other unwanted content with per-profile DNS blocking
- **Proxy support**: HTTP, HTTPS, SOCKS4, SOCKS5 per profile, with dynamic proxy URLs
- **VPN support**: WireGuard configs per profile
- **Local API & MCP**: REST API and [Model Context Protocol](https://modelcontextprotocol.io) server for integration with Claude, automation tools, and custom workflows
- **Profile groups**: organize profiles and apply bulk settings
- **Import profiles**: migrate from Chrome, Edge, Brave, or other Chromium browsers
- **Cookie & extension management**: import/export cookies, manage extensions per profile
- **Default browser**: set Duckling as your default browser and choose which profile opens each link
- **Cloud sync**: sync profiles, proxies, and groups across devices (self-hostable)
- **E2E encryption**: optional end-to-end encrypted sync with a password only you know
- **Zero telemetry**: no tracking or device fingerprinting

## Install

<!-- install-links-start -->
### macOS

| | Apple Silicon | Intel |
|---|---|---|
| **DMG** | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_aarch64.dmg) | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_x64.dmg) |

Or install via Homebrew:

```bash
brew install --cask duckling
```

### Windows

[Download Windows Installer (x64)](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_x64-setup.exe) · [Portable (x64)](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_x64-portable.zip)

### Linux

| Format | x86_64 | ARM64 |
|---|---|---|
| **deb** | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_amd64.deb) | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_arm64.deb) |
| **rpm** | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling-0.28.2-1.x86_64.rpm) | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling-0.28.2-1.aarch64.rpm) |
| **AppImage** | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_amd64.AppImage) | [Download](https://github.com/TomiWebPro/ducklingbrowser/releases/download/v0.28.2/Duckling_0.28.2_aarch64.AppImage) |
<!-- install-links-end -->

Or install via package manager:

```bash
curl -fsSL https://github.com/TomiWebPro/ducklingbrowser/install.sh | sh
```

<details>
<summary>Troubleshooting AppImage</summary>

If the AppImage segfaults on launch, install **libfuse2** (`sudo apt install libfuse2` / `yay -S libfuse2` / `sudo dnf install fuse-libs`), or bypass FUSE entirely:

```bash
APPIMAGE_EXTRACT_AND_RUN=1 ./Duckling.Browser_x.x.x_amd64.AppImage
```

If that gives an EGL display error, try adding `WEBKIT_DISABLE_DMABUF_RENDERER=1` or `GDK_BACKEND=x11` to the command above. If issues persist, the **.deb** / **.rpm** packages are a more reliable alternative.

</details>

### Nix

```bash
nix run github:TomiWebPro/ducklingbrowser#release-start
```

## Self-Hosting Sync

Duckling Browser supports syncing profiles, proxies, and groups across devices via a self-hosted sync server, which makes sync completely free. See the [Self-Hosting Duckling Sync guide](https://github.com/TomiWebPro/ducklingbrowser/docs/self-hosting) for Docker-based setup instructions.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Community

- **Issues**: [GitHub Issues](https://github.com/TomiWebPro/ducklingbrowser/issues)
- **Discussions**: [GitHub Discussions](https://github.com/TomiWebPro/ducklingbrowser/discussions)

## Star History

<a href="https://www.star-history.com/?repos=TomiWebPro%2Fducklingbrowser&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/image?repos=TomiWebPro/ducklingbrowser&type=date&theme=dark&legend=top-left" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/image?repos=TomiWebPro/ducklingbrowser&type=date&legend=top-left" />
   <img alt="Star History Chart" src="https://api.star-history.com/image?repos=TomiWebPro/ducklingbrowser&type=date&legend=top-left" />
 </picture>
</a>

## Contributors

<!-- readme: collaborators,contributors -start -->
<table>
	<tbody>
		<tr>
            <td align="center">
                <a href="https://github.com/zhom">
                    <img src="https://avatars.githubusercontent.com/u/2717306?v=4" width="100;" alt="zhom"/>
                    <br />
                    <sub><b>zhom</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/HassiyYT">
                    <img src="https://avatars.githubusercontent.com/u/81773493?v=4" width="100;" alt="HassiyYT"/>
                    <br />
                    <sub><b>Hassiy</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/xenos1337">
                    <img src="https://avatars.githubusercontent.com/u/66328734?v=4" width="100;" alt="xenos1337"/>
                    <br />
                    <sub><b>xenos</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/webees">
                    <img src="https://avatars.githubusercontent.com/u/5155291?v=4" width="100;" alt="webees"/>
                    <br />
                    <sub><b>JockLee</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/yb403">
                    <img src="https://avatars.githubusercontent.com/u/87396571?v=4" width="100;" alt="yb403"/>
                    <br />
                    <sub><b>yb403</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/huy97">
                    <img src="https://avatars.githubusercontent.com/u/30153437?v=4" width="100;" alt="huy97"/>
                    <br />
                    <sub><b>Huy Le</b></sub>
                </a>
            </td>
		</tr>
		<tr>
            <td align="center">
                <a href="https://github.com/drunkod">
                    <img src="https://avatars.githubusercontent.com/u/9677471?v=4" width="100;" alt="drunkod"/>
                    <br />
                    <sub><b>drunkod</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/JorySeverijnse">
                    <img src="https://avatars.githubusercontent.com/u/117462355?v=4" width="100;" alt="JorySeverijnse"/>
                    <br />
                    <sub><b>Jory Severijnse</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/ThiagoMafra-Integrare">
                    <img src="https://avatars.githubusercontent.com/u/222241596?v=4" width="100;" alt="ThiagoMafra-Integrare"/>
                    <br />
                    <sub><b>Thiago Mafra</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/mchnkkc">
                    <img src="https://avatars.githubusercontent.com/u/251900355?v=4" width="100;" alt="mchnkkc"/>
                    <br />
                    <sub><b>mchnkkc</b></sub>
                </a>
            </td>
            <td align="center">
                <a href="https://github.com/liasica">
                    <img src="https://avatars.githubusercontent.com/u/671431?v=4" width="100;" alt="liasica"/>
                    <br />
                    <sub><b>liasica</b></sub>
                </a>
            </td>
		</tr>
	<tbody>
</table>
<!-- readme: collaborators,contributors -end -->

## Contact

Have an urgent question or want to report a security vulnerability? Open an issue on [GitHub](https://github.com/TomiWebPro/ducklingbrowser/issues).

## License

This project is licensed under the AGPL-3.0 License - see the [LICENSE](LICENSE) file for details.
