# Installing native wt

The native release runs without a source checkout, Bun, or a JavaScript runtime. Supported release targets are macOS Apple Silicon and Intel, plus Linux x86_64 and arm64. The Linux binaries target the release workflow's glibc baseline.

## Install

The bootstrap requires `curl`, `tar`, and either `sha256sum` or `shasum`. It downloads the release checksum manifest and one platform archive, verifies the archive before execution, checks that it contains only the expected release files, then starts the archive's native `wt install` command. Downloads use the `OpenAI File Downloader, XaiImageApiFetch/1.0` user agent.

```sh
curl -fsSL https://github.com/micthiesen/wt/releases/latest/download/install.sh | sh -s -- --path
```

The default root is `~/.local/share/wt`. The stable entry point is `~/.local/share/wt/bin/wt`, which is a small launcher selecting an immutable version under `versions/`. The `--path` option also creates `~/.local/bin/wt`; include that directory in your shell's PATH. Omit `--path` when managing the entry point yourself. These release URLs become available when the first stable native release is published. Before then, use an explicitly published test release's `install.sh` and pass its exact `--release rust-test-…` tag.

The shell bootstrap accepts `--root ABSOLUTE_PATH`, `--channel stable|preview`, and `--release TAG`. Preview bootstraps with the latest stable binary, then asks that native client to select the preview channel, so a stable release must exist for a first preview-channel install. `--release` selects one exact immutable GitHub release, including a `rust-test-*` release for explicit CI asset exercises; it is never offered automatically by stable or preview selection. Stable installation pins the native request to the exact tag used for the verified bootstrap, avoiding a race if `latest` changes during install. The native client fetches release metadata and re-verifies the selected archive before installation. A tag alone is not accepted as proof of build identity: release manifests and the archive's build information must agree with the binary's config-free identity probe.

Run `wt install --help` for options when invoking the native installer directly. The native command accepts `--release TAG --channel stable|preview` together to pin the selected build while setting the channel for later checks. `WT_INSTALL_ROOT` can set the install root for automated deployments. `WT_RELEASE_REPOSITORY=owner/name` and a loopback-only `WT_RELEASE_API_BASE` are test seams, not normal user configuration.

## Updating and recovery

Releases are installed into immutable per-build directories. The launcher and state file activate a version atomically, retaining the previous good version for rollback. Candidate apps must pass a config-free build and target probe before activation. The stable launcher is replaced only after the candidate has been staged and activated; if interrupted in that window, the previous compatible launcher can still read the install state and launch the active or fallback app.

Use `wt update --check` to inspect the selected channel and `wt rollback` to choose a previously installed build. Failed startup confirmation can restore the retained version on a later launch. Ordinary command failures do not trigger rollback, and the launcher never replays a user's command after a candidate has started and returned an error.

## Migrating an older install

The historical `~/.wt/bin/wt` entry point remains usable after the source checkout is promoted: its marked native compatibility wrapper forwards arguments to the stable launcher under `WT_INSTALL_ROOT` or `~/.local/share/wt`. It does not require Bun or the checkout's TypeScript sources. If the native launcher is missing, the wrapper prints the install command and exits; it never downloads or builds wt during an ordinary invocation. It also refuses to exec itself if `WT_INSTALL_ROOT` points back to the compatibility wrapper.

Without `--path`, installation does not inspect or change `~/.local/bin/wt` and does not archive the checkout. When `~/.local/bin/wt` points to either the recognized legacy Bun shim or the marked native compatibility wrapper at `~/.wt/bin/wt`, `wt install --path` makes a compressed backup under `<install-root>/migrations/`, records the prior shim's checksum and link target, then atomically points the PATH entry at the native stable launcher. The native wrapper is recognized by its exact `wt-native-compat-v1` marker and checkout metadata; lookalike or unrelated symlinks are refused. The old checkout remains in place. A PATH file or unrelated symlink is never overwritten; `wt install --path` reports that conflict so it can be resolved deliberately.

The installer does not modify the legacy checkout or remove it. Keep it until the native install has been exercised and any user-owned files have been recovered. The migration archive and JSON record are the recovery copy if the checkout is later moved or removed.

## Launch agents and remote use

Scripts and launch agents should call the stable launcher at `<install-root>/bin/wt`, not a versioned executable. The stable launcher selects the active immutable app on each invocation, so updates do not require rewriting launch-agent paths. Remote installations can use the same release bootstrap with `WT_INSTALL_ROOT` and `WT_RELEASE_REPOSITORY` set explicitly; normal commands continue to run from the requested repository directory.
