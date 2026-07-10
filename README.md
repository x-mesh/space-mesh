# space-mesh

macOS disk space analyzer with a SwiftUI interface and a Rust scanning core.

## Homebrew

```bash
brew tap x-mesh/tap
brew install --cask x-mesh/tap/space-mesh
```

The cask installs `space-mesh.app` in the configured Homebrew app directory and
links the bundled `space-mesh` CLI into the Homebrew `bin` directory.

Upgrade or remove it with:

```bash
brew upgrade --cask x-mesh/tap/space-mesh
brew uninstall --cask x-mesh/tap/space-mesh
```

## Build

Requirements: macOS 14 or newer, Xcode, Swift 5.9 or newer, and stable Rust.

```bash
make test
make package
```

`make package` creates an ad-hoc signed application archive under `dist/`.
Pushing a tag matching the workspace version, such as `v0.1.0`, publishes ARM64
and Intel archives and updates `x-mesh/homebrew-tap` automatically.

The release workflow requires the `HOMEBREW_TAP_GITHUB_TOKEN` Actions secret to
have Contents write access to `x-mesh/homebrew-tap`.
