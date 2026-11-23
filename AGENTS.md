# Repository agent instructions

- Read `docs/macos26-intel-nvidia-rdp.md` before changing build, RDP, display,
  capture or video-encoding code. Keep work aligned with its Intel macOS 26 and
  NVIDIA validation goals.
- Keep Rust responsible for RDP and Swift responsible for macOS APIs. Do not
  add vendor-specific GPU assumptions to the RDP protocol core.
- Treat x86_64 compilation, macOS 26 runtime behavior and NVIDIA hardware
  encoding as separate claims that each need their own evidence.
- Follow the clean-room rule in `CONTRIBUTING.md`; use published protocol/API
  specifications and appropriately licensed open-source code.
- Keep changes focused and upstreamable. Rebase this personal branch on
  `upstream/main` after fetching updates; do not push to `upstream`.
- Run the relevant checks from `CONTRIBUTING.md` and report any unavailable
  toolchain or unverified hardware behavior.
- 需要提权时，先向用户询问密码。
