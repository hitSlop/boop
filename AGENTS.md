# Using Boop (`boop`) as a Coding Agent Across Git Worktrees

You are running locally on a macOS machine (often 8–16 GB RAM). **Do not run `xcodebuild` or heavy `swift build` locally** when `boop` is configured — it will exhaust local RAM and thrash swap. Use `boop` to compile remotely on Linux while keeping editing, searching, git, signing, and iPhone deployment local.

Binary path: `~/.local/bin/boop` (ensure `export PATH="$HOME/.local/bin:$PATH"` if not already in `PATH`).

---

## Golden Rules for Agents

1. **Edit locally, build remotely.** Keep file edits, `rg`/`grep`, `git`, and tiny Python/shell checks on the local Mac. Use `boop` for iOS compilation, SwiftPM builds/tests, and deploying to the iPhone.
2. **Never silently fall back to local `xcodebuild`.** If `boop build` or `boop run` fails with compiler errors, read the diagnostics, fix the Swift code locally, and re-run `boop`. Only use local `xcodebuild` if the user explicitly asks for macOS-only `XCUITest`s or Xcode Instruments.
3. **Never put secrets in tracked files.** Boop hard-excludes `.env*`, `*.p8`, `*.p12`, `*.pem`, `*.key`, `*.mobileprovision`, and SSH keys, and never uploads Apple signing credentials to the remote Linux VM (all signing happens locally on macOS).
4. **Always prefer `--json` when parsing results programmatically.**

---

## Spinning Up an Isolated Worktree with Boop

When starting a new feature/bugfix task in a Git worktree:

```bash
export PATH="$HOME/.local/bin:$PATH"

# 1. Create an isolated Git worktree from the main repo
MAIN_REPO="/path/to/your/ios-repo"
WT_DIR="/path/to/your/ios-repo-wt-<task-slug>"
git -C "$MAIN_REPO" worktree add -b "<branch-name>" "$WT_DIR" HEAD

# 2. Ensure .boop.toml exists in the new worktree (copy from main repo if untracked)
if [ ! -f "$WT_DIR/.boop.toml" ] && [ -f "$MAIN_REPO/.boop.toml" ]; then
  cp "$MAIN_REPO/.boop.toml" "$WT_DIR/.boop.toml"
fi

# 3. Verify local + remote health from inside the new worktree
cd "$WT_DIR"
xcodegen generate  # for projects using XcodeGen
boop pool status --json  # pooled workers
```

### Worktree forks

With `[workers.default.pool]` in the global config, run `boop pool warm --json` from the project's main checkout to build and stop the base. Each worktree leases a fork on its first build, run, publish, or exec. The project directory has the same absolute path on base and fork. Subsequent jobs resume and reuse the leased fork. Single-sandbox workers use their original worktree directories.

Run `xcodegen generate` in a fresh XcodeGen worktree and verify optional resource references (such as an untracked `GoogleService-Info.plist`). Inspect `boop pool status --json` for fork IDs, lease owners and last jobs. New leases wait in a FIFO queue at the cap. Use `--no-wait` for an immediate result.

Allow ~3–4 minutes for the very first build on a brand-new fork of a large app while Boat completes disk hydration (`hydration_ms` + `stamps_restore_ms`); subsequent incremental builds on that leased fork typically finish in 15–30 seconds. Compare `build_started_at` and `build_finished_at` across job JSON files to verify concurrent builds.

## Core Commands for Agents

### Compile-Check Only (Fastest Verification Loop — No iPhone Required)
Use this while iterating on code changes before asking the user to test on their phone:
```bash
cd "$WT_DIR"
boop build --json
```
- Syncs all tracked, modified, and untracked non-ignored files.
- `--json` emits one object on success or failure. Use `stage`, `failure_kind`, `retryable`, `diagnostics` and `log_path` to diagnose failures. Compare `sandbox_id`, `build_dir_warm`, `queue_ms`, `fork_ms`, `build_ms` and `pull_ms` across jobs.
- Each job has a separate result under `~/.local/state/boop/jobs/`.

### Build, Sign Locally & Launch Live on the User's iPhone
Use this when the change is ready for the user to see/test on their physical iPhone:
```bash
cd "$WT_DIR"
boop run --device --json
```
What happens automatically:
1. Snapshots & `rsync`s local dirty/untracked edits to the isolated remote worktree (~1.5s incremental).
2. Compiles on Boat Linux (~17–30s incremental on 4 vCPU).
3. Delta-pulls `<App>.app` back to `.boop/artifacts/<App>.app` (~2.5s incremental).
4. Compiles `.metal` shaders (`xcrun metal`) and app icon / asset catalogs (`xcrun actool` → `Assets.car` + `AppIcon*.png`) locally via SHA-256 caches (`<5ms` warm).
5. Signs embedded frameworks + `.app` locally using macOS Keychain & CMS-verified provisioning profile (~1.7s).
6. Installs & launches on the paired iPhone via `xcrun devicectl` (~4s).

Other useful `run` flags:
- `boop run --device "My iPhone"` — explicit device name or UDID
- `boop run --simulator <UDID>` — cross-compiles `arm64-apple-ios-simulator`, ad-hoc signs, installs & launches on the booted local iOS Simulator
- `boop run --device --config release` — Release build
- `boop run --device --no-launch` — build, sign, and install without launching
- `boop build --clean` — wipe this worktree's remote `DerivedData` (`xtool/.xtool-tmp`) and rebuild cleanly

### Publish an Untethered OTA Build to Firebase App Distribution (Build Inbox)
Use this when your worktree change is ready for the user to browse and install over-the-air on their iPhone without plugging it in:
```bash
cd "$WT_DIR"
boop publish \
  --name "New keyboard animation" \
  --notes "Spring transition and haptic tuning" \
  --json
```
What happens automatically:
1. Builds remotely on this worktree's leased Boat Linux fork (`debug` by default to reuse warm caches; pass `--config release` for Release).
2. Delta-pulls `.app` locally, compiles hash-cached `.metal` shaders + `[assets]` (`xcrun actool` → `Assets.car` + `AppIcon`), and stamps a unique `CFBundleVersion` in `.app/Info.plist` (without dirtying Git).
3. Signs locally via macOS Keychain (`auto` prefers an Ad Hoc distribution profile and falls back to Development; zero Apple credentials ever touch Boat).
4. Packages a clean `Payload/<App>.app` `.ipa` via `ditto --norsrc` and uploads to Firebase App Distribution using the local `firebase` CLI (`--groups` / `app_id` come from `[distribution.firebase]` in `.boop.toml` or `GoogleService-Info.plist`).
5. Saves the build record (`build_id`, `testing_uri`, `branch`, `commit`, `worktree_id`, `ipa_path`) under `~/.local/state/boop/builds/` and returns `testing_uri` in `--json` so you can hand the user a direct install link.

Inspect published and named builds across all worktrees:
```bash
boop builds            # human-readable build inbox table
boop builds --json     # structured JSON for agents/UI
```
Use `boop publish --no-build` to re-sign, package, and upload the already-pulled `.boop/artifacts/<App>.app` without hitting Boat.

### Run Linux-Compatible Unit Tests or Commands Remotely
```bash
boop exec -- swift test --package-path Packages/MyCore
```
Pooled workers execute in the leased fork's project directory. Hosted XCTest and UI tests require an authorized Xcode run on the Mac.

---

## Lifecycle, Cleanup & Cost Hygiene

```bash
# Inspect worker status, Boat spend, and recent job IDs/timings:
boop status --json

# Pause the Boat VM when your session is done (halts compute billing at $0/hr; keeps disk & caches for 14 days):
boop stop
```

When cleaning up a finished worktree:
```bash
cd "$WT_DIR"
boop release --json
cd "$MAIN_REPO"
git worktree remove "$WT_DIR"
```

---

Use `boop pool gc` to reclaim forks after worktree paths have already been removed. `boop stop` pauses idle forks and the base. Stopped leases count toward the cap.

## Quick Troubleshooting for Agents

| Symptom | Cause & Fix |
| :--- | :--- |
| `No .boop.toml found` | Copy `.boop.toml` from the main repo into the worktree root, or run `boop init --boat-id <sandbox-id>`. |
| `Physical iOS device not found` / `devicectl` install fails | Phone is locked or disconnected. Fall back to `boop build --json` to verify compilation, and ask the user to unlock their iPhone before `boop run --device`. |
| `Worker is missing iOS toolchain/shims` | Boop automatically runs self-healing bootstrap + SDK upload. Wait for it to finish once. |
| `boop publish` uses Development instead of Ad Hoc | Run `boop doctor` to check `adhoc_profile`. Ensure an `Apple Distribution` cert is in Keychain and the Ad Hoc `.mobileprovision` is copied into `~/Library/Developer/Xcode/UserData/Provisioning Profiles/<UUID>.mobileprovision`. |
| Need full compiler output | Re-run with `boop build --verbose` (without `--json`). |
| Boat trial quota / need faster builds | Upgrade Boat plan and create a `large` (8 vCPU / 16 GB, `$0.072/hr`) worker with `boop init --worker-name large --provider boat --machine-type large --bootstrap`. |
