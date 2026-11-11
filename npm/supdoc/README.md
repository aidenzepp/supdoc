# supdoc

supdoc tracks links from documentation to code and reports when the linked code changes.

This package installs the prebuilt `supdoc` binary for your platform, so a JavaScript or TypeScript project can pin supdoc in its own lockfile. Each binary is the one in the matching [GitHub release](https://github.com/aidenzepp/supdoc/releases) archive, unchanged. No package runs an install script, so installs work with `--ignore-scripts` and with pnpm and Bun defaults. The [supdoc repository](https://github.com/aidenzepp/supdoc) documents the commands and configuration.

## Install

```bash
npm install --save-dev supdoc
pnpm add -D supdoc
bun add -d supdoc
```

The package manager installs one of these optional dependencies, chosen by `os` and `cpu`:

| Platform | Package |
| --- | --- |
| macOS, Apple silicon | `@supdoc/darwin-arm64` |
| macOS, Intel | `@supdoc/darwin-x64` |
| Linux, arm64 | `@supdoc/linux-arm64` |
| Linux, x64 | `@supdoc/linux-x64` |
| Windows, arm64 | `@supdoc/win32-arm64` |
| Windows, x64 | `@supdoc/win32-x64` |

The Linux binaries are statically linked and run on glibc and musl distributions. Installing with `--omit=optional` or `--no-optional` leaves no binary to run. On any other platform, install supdoc with `cargo install supdoc --locked`.

## Start

```console
$ npx supdoc --version
$ npx supdoc --help
```

## Upgrade

Change the `supdoc` version in `package.json` and reinstall. `supdoc upgrade` refuses to replace a binary that npm installed and prints the package-manager command to run instead.

## License

MIT
