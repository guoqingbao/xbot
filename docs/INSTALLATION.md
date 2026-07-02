# Installation

`xbot` supports multiple installation methods across Linux, macOS, and Windows.

## One-Line Install (Recommended)

The easiest way to install `xbot` on Linux or macOS:

```bash
curl -sSL https://guoqingbao.github.io/xbot/install.sh | bash
```

This script:
- Auto-detects your operating system (Linux/macOS)
- Auto-detects your CPU architecture (x64/ARM64)
- Downloads the appropriate pre-built binary from GitHub Releases
- On Linux: offers deb package or binary install options
- On macOS: installs directly to `/usr/local/bin`

**Environment variable overrides:**

| Variable | Description |
|----------|-------------|
| `XBOT_PLATFORM` | Override platform detection (e.g., `linux-x64`, `darwin-arm64`) |
| `XBOT_MODE` | Skip interactive prompt (`1` = deb, `2` = binary) |

**Non-interactive install (e.g., in CI):**

```bash
# Install deb package non-interactively
curl -sSL https://guoqingbao.github.io/xbot/install.sh | XBOT_MODE=1 bash

# Install binary to /usr/local/bin non-interactively
curl -sSL https://guoqingbao.github.io/xbot/install.sh | XBOT_MODE=2 bash
```

## Debian / Ubuntu

Download the `.deb` for your architecture from the GitHub release page, then install it:

```bash
sudo apt install ./xbot-linux-x64.deb
```

or:

```bash
sudo dpkg -i ./xbot-linux-x64.deb
sudo apt-get install -f
```

The Debian package installs:

- `/usr/bin/xbot`
- `/usr/share/xbot/skills`
- README and license files under `/usr/share/doc/xbot`

No systemd service or `/etc` config is installed in the v1 package. Run `xbot onboard` after installation to create `~/.xbot/config.json` and the default workspace.

## npm

The npm package is published under the `trusted-ai` organization scope.

```bash
npm install -g @trusted-ai/xbot
xbot --help
```

The npm package installs a small Node.js launcher named `xbot`. During `postinstall`, it downloads the matching prebuilt native binary from GitHub Releases, verifies `SHA256SUMS`, and stores it under the package directory.

Supported npm binary targets:

- `linux-x64`
- `linux-arm64`
- `darwin-x64`
- `darwin-arm64`

Optional install environment variables:

- `XBOT_INSTALL_BASE_URL`: override the release artifact base URL
- `XBOT_INSTALL_VERSION`: override the package version used for artifact names
- `XBOT_INSTALL_TAG`: override the GitHub release tag

## Cargo

The crates.io package is published as `xbot`.

```bash
cargo install xbot
```

The installed binary is named `xbot`.

Before publishing a release, run:

```bash
cargo package --list
cargo publish --dry-run
```

## Source Checkout

Development installs still work with Cargo:

```bash
cargo run --release -- --help
cargo build --release
```

The source checkout automatically uses the repository `skills/` directory as the built-in skills location.

## Windows

Download the `.zip` archive for your architecture from [GitHub Releases](https://github.com/guoqingbao/xbot/releases/latest):

- `xbot-<version>-win32-x64.zip` — Windows x86_64
- `xbot-<version>-win32-arm64.zip` — Windows ARM64

Extract and add to your `PATH`:

```powershell
# Extract to a directory
Expand-Archive xbot-*-win32-x64.zip -DestinationPath $env:USERPROFILE\.xbot\bin

# Add to PATH (PowerShell)
$env:PATH += ";$env:USERPROFILE\.xbot\bin"

# Verify installation
xbot --help
```

## Supported Platforms Summary

| Platform | Architecture | Install Methods |
|----------|-------------|-----------------|
| Linux | x86_64 | install.sh, .deb, npm, cargo, source |
| Linux | ARM64 | install.sh, .deb, npm, cargo, source |
| macOS | Apple Silicon | install.sh, npm, cargo, source |
| macOS | Intel x64 | install.sh, npm, cargo, source |
| Windows | x86_64 | .zip, npm, cargo, source |
| Windows | ARM64 | .zip, cargo, source |
