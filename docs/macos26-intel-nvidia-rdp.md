# macOS 26 Intel + NVIDIA direction

This branch targets ClawTank running on an Intel Mac with macOS 26 and an NVIDIA
GPU. The work is intentionally split into build portability, macOS runtime
compatibility and RDP performance. A passing build does not prove that the
NVIDIA encoder path is available.

## Branch contract

- `main` remains the local mirror of the upstream baseline.
- `macos26-intel-nvidia-rdp` is the personal integration branch and tracks
  `upstream/main` as its rebase point.
- Feature work should use short-lived topic branches or focused commits with
  one concern per change. Do not commit generated Xcode projects, build
  products or local signing material.
- Before opening a pull request or incorporating upstream work:

  ```sh
  git fetch upstream
  git rebase upstream/main
  make lint test
  ```

- Keep protocol changes, macOS integration changes and performance tuning in
  separate commits when they can be reviewed independently. This keeps the
  branch easy to rebase and makes upstream cherry-picks practical.

## Current baseline

The repository uses a Rust RDP core and a Swift macOS host. The host video path
is ScreenCaptureKit → VideoToolbox → RDP EGFX AVC420. ClawTank has no direct
NVENC/CUDA backend; `H264Encoder` asks VideoToolbox for a hardware encoder. A
community driver project, NullMoth, currently lists x86_64 macOS 26 and NVIDIA
Turing-and-newer cards as targets. Its own 2.9 parity audit reports H.264/HEVC
hardware encoding through VideoToolbox across listed GPU generations. That
gives this branch a plausible route through the existing encoder API, subject
to validation with ClawTank's exact encoder settings and RDP pipeline. The
audit is project-reported evidence: its media-service selection still needs
second-device testing, so it does not prove ClawTank compatibility.

Apple officially supports macOS 26 on a short list of Intel Macs: the 2019
16-inch MacBook Pro, 2020 four-port 13-inch MacBook Pro, 2020 27-inch iMac and
2019 Mac Pro. See [Apple's compatibility list](https://support.apple.com/en-us/122867).
NullMoth is a third-party, OpenCore-based driver project, not an Apple- or
NVIDIA-supported configuration. Its setup requires kernel extensions and
non-default OpenCore boot and security settings. Record the exact GPU, driver
build and boot configuration for every test. Its plugin, kernel extensions and
packaging components use PolyForm Noncommercial 1.0.0 (the translator has a
separate LGPL-3.0 license); do not copy or redistribute those components as
part of ClawTank.

Current local build probe (2026-11-23): Mac Pro (MacPro7,1), Intel Xeon W,
32 GB RAM and NVIDIA GeForce RTX 5060 (8 GB), running macOS 26.7.3 build
25G342. The host is `x86_64`; Swift 6.3.3 targets
`x86_64-apple-macosx26.0`, and the installed macOS SDK is 26.5. The GPU is
visible to macOS with Metal 4. The reported CPU core counts differ between
system interfaces, so this record omits a core count.

Initial build probe: `make ARCH=x86_64 core` stopped because Cargo was absent.
MacPorts 2.12.7 is available and currently has no installed ports. Rust,
XcodeGen and SwiftLint are being bootstrapped with MacPorts. The active developer
directory is `/Library/Developer/CommandLineTools`; full Xcode and `xcodebuild`
are not installed. Rust core compilation can be checked after bootstrapping, but
the Swift app build and XCTest remain blocked until full Xcode is installed.
These are environment blockers, not compiler errors in the project.

The build defaults to the host architecture. On an Intel Mac this naturally
selects `x86_64` for both Cargo and Xcode. Cross-building an Intel app from
Apple silicon is a separate task: the Rust static library must also be built
for `x86_64-apple-darwin`; setting only Xcode's `ARCHS` is insufficient.
The current Makefile does not pass a Cargo target, so `make build ARCH=x86_64`
is a native-Intel path today, not a complete Apple-silicon cross-build path.
The general app target has no arm64-only source restriction, but the current
release helper packages `clawtank-mcp` as an arm64-only MCP Bundle. An Intel
release therefore needs either a separate MCP bundle policy or a packaging
change validated with the registry consumers.

## Acceptance matrix

1. **Build:** complete Xcode, Rust stable (project MSRV 1.88), XcodeGen and
   SwiftLint are installed; `make build ARCH=x86_64` succeeds; the app and
   bundled MCP binary report `x86_64` with `lipo -archs`.
2. **Tests:** `make lint`, `make test ARCH=x86_64` and the FreeRDP interoperability
   smoke test pass. Tests that need a real display or permissions are recorded
   separately as manual results.
3. **macOS 26 runtime:** Screen Recording and Accessibility work, virtual
   display creation and resize work, audio/clipboard/input work, and the app
   survives sleep, wake and display changes.
4. **Video:** log the VideoToolbox encoder identifier, hardware-acceleration
   result, frame rate, encode latency, bitrate and CPU/GPU load. Test both
   physical and virtual displays.
5. **RDP interoperability:** validate Windows App or `mstsc`, FreeRDP 3 and a
   second macOS client for NLA, resize, AVC420, cursor, input, audio and
   clipboard. Record client, GPU, driver and macOS build for every result.

For encoder evidence, query the VideoToolbox encoder state and record the
encoder identifier and `UsingHardwareAcceleratedVideoEncoder` result. Apple's
[VideoToolbox documentation](https://developer.apple.com/documentation/videotoolbox/kvtcompressionpropertykey_usinghardwareacceleratedvideoencoder)
defines that property as the runtime indication that hardware encoding is in
use. Do not infer that a registered NVIDIA encoder is selected just because
the display is accelerated. Test the exact Tahoe build, GPU and driver because
the community parity audit is not a ClawTank test result.

## Engineering boundaries

- Preserve the Rust/Swift boundary: Rust owns RDP and Swift owns macOS APIs.
- Keep NVIDIA-specific code behind a small video-encoder abstraction. Do not
  make the RDP protocol depend on a vendor API.
- If VideoToolbox cannot provide a hardware encoder on an Intel/NVIDIA system,
  first add explicit detection, diagnostics and a tested fallback policy. Do
  not silently claim hardware acceleration.
- Keep `CGVirtualDisplay` isolated behind `VirtualDisplayProvider`; macOS 26
  behavior needs a real-machine test because it uses a private API.
- Use published Apple/RDP documentation and upstream open-source code only,
  consistent with the clean-room rule in `CONTRIBUTING.md`.

## First implementation sequence

1. Establish a reproducible x86_64 build and test environment on macOS 26.
2. Decide whether Intel is a supported release target; if so, remove the
   arm64-only MCP packaging assumption with a compatibility-tested change.
3. Add an x86_64 CI or self-hosted verification job without changing the
   default Apple-silicon release path.
4. Test the existing VideoToolbox hardware-required path on the target GPU and
   inspect encoder selection and sustained RDP output. Add an encoder fallback
   abstraction only if measurements show the existing path cannot meet the goal.
5. Fix only measured RDP regressions, keeping each protocol or host change
   independently reviewable and upstreamable.
