# Installation

`xbot` supports direct installation without cloning the repository.

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
