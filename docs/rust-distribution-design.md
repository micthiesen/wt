# Native release and update design

## Release contract

The update client reads GitHub Releases through the configured repository. Stable uses GitHub's `releases/latest`; preview selects the newest non-draft prerelease tagged `preview-<full commit SHA>` from the first 30 releases. Test releases are excluded from automatic discovery. Every release must include `wt-release.json` with schema version 1:

```json
{
  "schema_version": 1,
  "release_version": "v1.2.3",
  "build_id": "<full commit SHA>",
  "artifacts": [
    {
      "target": "aarch64-apple-darwin",
      "filename": "wt-v1.2.3-aarch64-apple-darwin.tar.gz",
      "sha256": "<64 lowercase hex characters>",
      "size": 123456
    }
  ]
}
```

The updater requires this manifest, matches its release version to the GitHub tag and every artifact record to the GitHub asset name and size, bounds metadata and downloads, then verifies the archive SHA-256 before extraction. The manifest's `build_id` is kept separate from its release tag. Each archive contains exactly these regular files beneath the archive-stem directory:

- `wt`: native application
- `wt-launcher`: launcher payload for installer/bootstrap compatibility
- `wt-build-info.json`: `{ "schema_version": 1, "release_version": "v1.2.3", "build_id": "<full commit SHA>", "target": "aarch64-apple-darwin" }`

The archive build-info must agree with the release manifest and requested target. The app's config-free `--_boot-probe` prints exactly `wt-build-id:<build_id>:<target>` on stdout; the launcher compares that output before sending user arguments. CI must build immutable preview tags per commit and must not replace assets behind a tag. The HTTP client uses `OpenAI File Downloader, XaiImageApiFetch/1.0`.

`cargo-dist` is a reasonable archive/checksum/release-workflow generator, and its generated archives use the root-directory layout expected here. It does not provide wt's boot protocol, immutable activation, or rollback policy. Its optional standalone updater is an axoupdater program for the standard installer layout, which does not fit wt's stable launcher and release manifest contract. Use cargo-dist for artifact construction only if CI can reliably add the required build-info and release manifest; otherwise retain a small project-owned archive/release job. See [cargo-dist checksum and archive settings](https://axodotdev.github.io/cargo-dist/book/reference/config.html) and [updater settings](https://axodotdev.github.io/cargo-dist/book/reference/config.html).

## Install tree and activation

The stable entry point remains `<install-root>/bin/wt`, so launchd plists and remote callers keep one path. Mutable updater state is in `<install-root>/state.json`; a per-user OS file lock at `update.lock` serializes only state changes. Verified builds are immutable under `versions/<release-version>-<build-id>-<target>/bin/`. Downloads and extraction happen under `staging/`; the engine accepts verified in-memory artifacts, never an arbitrary archive path.

The engine writes and syncs both executables into staging, atomically renames the completed directory into `versions/`, then durably records the pending build and its prior last-good version. A crash before activation leaves the previous version selected; a complete but unselected version can be recovered on a later install. Activation never points at a directory that has not finished installing. Old versions are retained; pruning is a separate policy and must never remove current, last-good, or pending versions.

The launcher holds the state lock only while reading or changing state. It runs the candidate's config-free probe without user arguments. A failed candidate probe rejects that pending build and selects last-good before dispatch. Only after a probe succeeds does it start the real application with the original argv, exactly once. A normal command's exit status never triggers rollback or replay. The app confirms the pending version after repository-independent runtime and signal setup, before configuration, logging, or database migration. A bad repository configuration cannot reject a binary globally. Concurrent confirmations of the same build and token are idempotent under the state lock. Explicit failure of the repository-independent bootstrap can roll the candidate back. Existing database/state migration rules still govern whether last-good remains safe to execute.

The launcher exports `WT_INSTALL_ROOT`, `WT_INSTALL_VERSION`, `WT_BUILD_ID`, `WT_TARGET`, and (only during a pending boot) `WT_BOOT_ATTEMPT_TOKEN`. App confirmation and failure reporting must match the active build, release version, target, and attempt token. Stale confirmations fail closed. Operational command errors are not boot failures.

Declining an update stores its build SHA, so the same build stays suppressed while a newer build can be offered. Startup checks are due once per day; a future timestamp caused by clock rollback is treated as due. Channel and repository configuration are explicit inputs, with a local HTTP fixture seam; no module reads global config.

## Legacy install migration

A legacy source-checkout shim may be replaced only by the installer migration path. That path must first identify the expected legacy layout, make a durable backup of the prior launcher and source checkout, and leave unrelated files untouched. The stable entry-point path must remain usable for launchd and remote callers throughout migration. The updater engine itself never writes into a source checkout and does not infer that a path is safe to replace merely because it is named `wt`.

## Current implementation and release gates

`wt-update` implements bounded GitHub metadata/archive fetch, manifest/checksum and archive identity validation, safe fixed-path extraction, immutable staging, durable state and attempt transitions, decline tracking, and daily-check decisions. `wt-launcher` implements probe-before-argv dispatch and stable path invocation. Scoped tests use isolated directories and an in-process HTTP fixture; they do not install or replace a live binary.

The release CI and installer remain integration gates: CI must emit `wt-release.json` and `wt-build-info.json` exactly as specified, installer migration must back up a recognized legacy install, and app startup must wire the hidden probe plus success/failure callbacks at the pre-migration boundary. No live release with this manifest exists yet, so the server-side release schema has only local-fixture verification. Stable-launcher replacement and launchd/remote-path migration need an isolated installer test before native distribution is complete.
