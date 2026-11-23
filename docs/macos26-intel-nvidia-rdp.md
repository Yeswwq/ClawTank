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
is ScreenCaptureKit → VideoToolbox → RDP EGFX AVC420. There is no NVIDIA NVENC
or CUDA integration in the current code. `H264Encoder` asks VideoToolbox for a
hardware encoder, so NVIDIA support must be established by runtime evidence on
the target Mac rather than by the CPU architecture alone.

The build defaults to the host architecture. On an Intel Mac this naturally
selects `x86_64` for both Cargo and Xcode. Cross-building an Intel app from
Apple silicon is a separate task: the Rust static library must also be built
for `x86_64-apple-darwin`; setting only Xcode's `ARCHS` is insufficient.

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
2. Add an x86_64 CI or self-hosted verification job without changing the
   default Apple-silicon release path.
3. Instrument and test VideoToolbox on the target NVIDIA hardware; decide
   whether the existing path is sufficient or a fallback abstraction is needed.
4. Fix only measured RDP regressions, keeping each protocol or host change
   independently reviewable and upstreamable.
