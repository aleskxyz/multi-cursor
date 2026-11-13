# Contributing to Multi Cursor

Thanks for considering a contribution. Bug reports, documentation improvements, design feedback, and pull requests are all welcome.

## Before you start

- Read the [README](README.md) for prerequisites and the local development workflow.
- Keep changes focused. If a change affects how environments, accounts, or Cursor data are managed, explain the intended behaviour and safety considerations in the pull request.
- Do not commit any data from `~/.multi-cursor/`; account snapshots can contain authentication tokens.

## Development workflow

Install dependencies and run the app in development mode:

```bash
npm install
npm run tauri dev
```

Build a production app with:

```bash
npm run tauri build
```

For local installation details, see the README. For signed release builds and DMGs, see [macOS distribution](docs/macos-distribution.md).

## Testing changes safely

Multi Cursor manages real Cursor data. Test account and environment changes with disposable Cursor accounts and environments where possible. In particular, check that:

- Cursor is closed before changes that rename folders or update authentication data.
- A normal Dock or Spotlight launch opens the environment selected in Multi Cursor.
- Removing an inactive environment sends its stored data to Trash (Recycle Bin on Windows).

On Linux and Windows, also check that:

- Multi Cursor finds your Cursor install (or that `cursorAppPath` in `~/.multi-cursor/config.json` is honoured).
- Switching environments right after Cursor quits works. On Windows, lingering Cursor helpers or antivirus scans can briefly lock files; Multi Cursor retries the folder rename for a few seconds.
- Launch starts Cursor detached, so it keeps running after Multi Cursor quits.

`cargo test` in `src-tauri/` runs a switch test against a temporary home folder; it never touches your real Cursor data.

## Continuous integration

Every pull request and push to `main` runs [CI](.github/workflows/ci.yml): TypeScript and `cargo fmt` checks, then `cargo clippy` and `cargo test` on macOS, Linux and Windows. Run the same checks locally before opening a pull request:

```bash
npm run build                 # TypeScript check + frontend build (the Rust crate embeds it)
cd src-tauri
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --lib --locked
```

The Rust version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) and the Node version in [`.nvmrc`](.nvmrc); `rustup` picks up the Rust version automatically.

To get installable packages for every platform from any branch, run **Actions → Build → Run workflow**. The packages are attached to the run for 14 days.

## Releases

Versions follow [SemVer](https://semver.org): patch for fixes, minor for new features, major for changes older versions can't read (such as the `~/.multi-cursor/` layout). The version is stored in five files; [`scripts/version.sh`](scripts/version.sh) prints it and checks that they agree.

1. **Actions → Prepare release → Run workflow**, and enter the version (e.g. `1.2.0`, or `1.2.0-beta.1` for a pre-release). It opens a "Release v1.2.0" pull request that sets the version everywhere. This needs **Settings → Actions → General → Allow GitHub Actions to create and approve pull requests**.
2. **Merge the pull request.** The [Release workflow](.github/workflows/release.yml) then runs CI, builds every platform and creates a **draft** release with the packages, `SHA256SUMS.txt` and build provenance.
3. **Review the draft and click Publish.** Publishing creates the `v1.2.0` tag. It is also what the app's **Check for updates** sees; pre-releases are never offered as updates.

Nothing is public before step 3, so a failed build leaves nothing to clean up: fix it and merge, or rerun **Actions → Release**, which refreshes the draft for the latest commit. Never move a published tag; release a new patch version instead.

macOS builds are signed and notarized only if the repository has the signing secrets described in [macOS distribution](docs/macos-distribution.md#sign-in-github-actions); Windows builds are unsigned.

## App icon

The branded icon source is [`assets/logo.png`](assets/logo.png). The main window uses a copy at `public/assets/logo.png`.

To regenerate the Tauri icon set (Dock, App Switcher, `.icns`, etc.):

```bash
npx tauri icon assets/logo.png
# Keep the in-app header icon in sync (128px is enough for the 48px UI slot):
sips -z 128 128 assets/logo.png --out public/assets/logo.png
```

Quit and relaunch Multi Cursor after changing icons; macOS caches Dock icons. A release build installed with `npm run install-app` is the most reliable way to check the final Dock appearance.
