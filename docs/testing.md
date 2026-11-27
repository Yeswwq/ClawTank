# Manual test checklist

Run the relevant section at the end of the phase noted. Record the date, macOS version, client and result.

## 本机实时日志

每次构建、启动、复测与日志清理都使用以下固定规则：先正常退出旧版 ClawTank；
执行 `make full-run ARCH=x86_64` 运行 lint、Rust/XCTest 和 Release 构建，全部成功后将新 App
替换到 `/Applications/ClawTank.app` 并启动；本轮
`runtime.log`、`runtime-errors.log` 和 `runtime-session.txt` 在启动前自动覆盖；
复现和客户端连接期间完成测试；问题发生后按日志时间、`category` 和
`processID` 分析。不要同时从 `/Applications`、Xcode Debug 产物或其他副本启动
同一 Bundle ID 的应用，否则统一日志、TCC 授权和故障现场会混在一起。

Swift 和 Rust 的运行日志通过 macOS 统一日志写入同一 subsystem：
`dev.clawtank.ClawTank`。Rust tracing 由 `CoreBridge` 转到 `core` category；其他
Swift category 包括 `server`、`display`、`audio`、`capture`、`encode`、`video`、`bitmap`、
`media`、`clipboard`、`files` 和 `agent`。连接与 Agent 操作审计另存为
`~/Library/Application Support/ClawTank/audit.log`，不要把它与统一日志采集文件混淆。

复现问题时使用 `make full-run`：它依次运行 lint、完整测试和当前 x86_64/arm64 Release
构建，验证通过后替换 `/Applications/ClawTank.app`，然后在启动新构建前清除
`~/Library/Application Support/ClawTank/` 下的 `runtime.log`、
`runtime-errors.log` 和 `runtime-session.txt`，然后通过 macOS `log stream`
以 NDJSON 格式实时采集上述 subsystem，并启动刚安装的 Release App。采集器过滤 `log stream`
开头的提示文本，只将带 `eventType` 的 JSON 事件写入日志，诊断输出单独写入错误文件。
`make run` 适合只需构建和部署、无需
重新执行完整检查的复测。为避免把旧进程的日志
混入本轮，若检测到已有 ClawTank 进程，脚本会请求应用正常退出；无法在 30 秒内退出时，
会停止部署并保留现有安装。
按 Ctrl-C 只会停止日志采集，不会退出应用。

`runtime-session.txt` 记录本轮运行 ID、启动时间、已安装 App 路径、主机架构、进程 ID
和可执行文件路径；`runtime.log` 是
本轮统一日志 NDJSON，可直接按时间戳、`category`、`processID` 和消息筛选；
`runtime-errors.log` 保存采集命令本身的错误输出。每次 `make full-run` 或 `make run` 会覆盖这三个
采集文件；审计日志和 macOS 统一日志数据库不受影响。
这覆盖主应用 Swift 日志及经 `CoreBridge` 转发的 Rust 日志。独立 MCP Server 的
stderr 由启动它的 MCP Host 管理，不属于 macOS 统一日志流。可用 Console.app 按
subsystem `dev.clawtank.ClawTank` 查看同一日志流。

## Phase 0
- [ ] `make spike-vd` on this Mac after granting Screen Recording; the PNG shows the virtual desktop.
- [ ] `make spike-vd` on the headless Mac mini (no monitor, no dummy plug).

## Phase 1 — first connection
Get the connection details from the ClawTank menu: **Copy Address**, the username shown there, and **Copy Password**.
- [ ] Windows App (Mac) → Add PC → PC name `127.0.0.1` (same Mac) or `<host>.local` (other device). User account = the menu's username and password. Connect, accept the certificate (compare the SHA-256 with **Copy Certificate Fingerprint**), and see the colour-bar test pattern.
- [ ] While connected, the menu shows **Connected: <ip>** and the menu-bar icon changes.
- [ ] Close the session window. The menu returns to **Ready**.
- [ ] A wrong password fails, and the menu shows **Failed login from <ip>**.
- [ ] (Optional) the same from Windows `mstsc.exe` or another device on the LAN.
- [x] (CI, first run 2026-10-21) FreeRDP 3 on Ubuntu authenticated with NLA, was refused with a wrong password, and connected and received the test frame. The final disconnect check failed only because the script killed the `xvfb-run` wrapper instead of FreeRDP; fixed.
- Automated check against the running app (no GUI client needed):
  `cd core && CLAWTANK_PASSWORD="$(cat ~/Library/Application\ Support/ClawTank/secrets/rdp-password.secret)" cargo run --example probe -- 127.0.0.1:3389 "$(whoami)"`
  Build it first with `cd core && cargo build --example probe`. It connects with TLS + NLA, verifies the test pattern and saves `probe.bmp`.
  - 2026-10-21, macOS 15.6.1, M2: ✅ loopback (1280×800) and LAN 192.168.1.20 (1512×982) verified; wrong password refused by CredSSP.

## Phase 2 — live video
Automated against the running app (Screen Recording granted to ClawTank):
`cd core && CLAWTANK_PASSWORD="$(cat ~/Library/Application\ Support/ClawTank/secrets/rdp-password.secret)" cargo run --example probe -- 127.0.0.1:3389 "$(whoami)" 1512x982 --egfx 10`
Set `CLAWTANK_DUMP_H264=path` to save the first access unit, which you can decode to verify.

Results, 2026-10-21, M2 MacBook Air, macOS 15.6.1, loopback, 1512×982 session:
- ✅ First frame 357–586 ms after connect. It starts with SPS, PPS, IDR, and decodes with VideoToolbox (1520×992 coded) to a legible image of the screen.
- ✅ 60 fps motion window (900×600): **57–58 fps** sustained, about 1.0 Mbit/s.
- ✅ ClawTank CPU during motion: **7–8%** of one core (target < 15%).
- ⚠️ Idle: by design no frames are sent for a static screen (ScreenCaptureKit `.idle`). It couldn't be measured strictly here because a terminal spinner kept changing (9 fps, 17 kbit/s with only that). The maintainer should re-check with nothing animating.
- `make bench` (synthetic 2560×1600 @ 60): rtvc low-latency p50 11.0 ms, p95 20.3 ms; ave.avc p50 11.6 ms, p95 37.5 ms (ADR-016).
- [ ] Human: Windows App shows the live desktop, window dragging is smooth, video playback looks right, text is crisp.
- [ ] Human: "Show Stats Overlay" draws fps/bitrate/encode time on the video.
- [ ] Not yet measured: end-to-end input→display latency.

2026-11-23，Intel Mac Pro（MacPro7,1）、macOS 26.7.3、RTX 5060/NVMTL 3.0：最新诊断包中的 TCC 预检为 granted，RDP 认证、音频和 EGFX 协商均成功；视频在低延迟 VideoToolbox 会话创建处失败。独立 API 探针确认 NVMTL 对低延迟 specification 返回 `-12902`，标准硬件会话报告 NVIDIA NVENC ID 和硬件加速 true。修复后的新构建尚未完成真实客户端视频复测，不能据 API 探针宣称 RDP 视频验收通过。

Automated reliable UDP path (`cargo test -p clawtank-core --test video`):
- ✅ 客户端协商 `UDP_FECR` + `SOFT_SYNC_TCP_TO_UDP`，在同一个端口上建立
  RDPEUDP2 + TLS + RDPEMT 侧带，服务端把 EGFX 迁移到该 tunnel。
- ✅ AVC420 帧经 UDP 到达客户端并产生图形更新，EGFX ACK 返回 `VideoHub`。
- ✅ EGFX 迁移后，Display Control resize 仍经 TCP 往返。
- ✅ 客户端公告 multitransport 但拒绝 UDP 时，AVC420 仍经 TCP 到达。
- ✅ ACK suspend 会清除旧的 in-flight 计时样本；强制 IDR 之后，早先生成的
  P-frame 输出会在交付队列中按 reference generation 丢弃。
- [ ] 人工：Windows App / `mstsc` 和 FreeRDP 在真实局域网中分别测试允许
  UDP 与防火墙阻断 UDP。记录客户端版本、侧带是否建立、EGFX 是否迁移，以及
  帧和 ACK 是否连续。

## Phase 3 — input and cursor
Automated live test (it takes over the mouse and keyboard for about 15 s; ask first): a centred test window plus
`probe … 1512x982 --input-demo OUTDIR`. Results, 2026-10-21, M2, macOS 15.6.1, Accessibility granted:
- ✅ Click at the session centre landed at the exact screen centre (735, 478 of 1470×956 pt).
- ✅ Scancodes typed `clawtank`; Shift+1 gave `!`; the Unicode event gave `é`. A Shift flag that leaked onto the Unicode event was found here and fixed.
- ✅ 3 wheel notches gave 3 continuous pixel scrolls (−40 each); the middle click arrived as button 2.
- ✅ Cursor shapes reached the client: arrow 17×24 (hotspot 4,4) and I-beam 9×18 (hotspot 4,9) over text, correct orientation.
- [ ] Human (Windows App / mstsc): type in a non-US layout with the same layout selected on the Mac; ⌘C/⌘V/⌘Tab (Windows key = ⌘ by default, or turn on "Swap Ctrl and ⌘"); trackpad scrolling feel; resize and hand cursors.

## Phase 4 — virtual display and resize
Results, 2026-10-21, M2 MacBook Air, macOS 15.6.1:
- ✅ Opt-in real-display test (`TEST_RUNNER_CLAWTANK_VD_TESTS=1 make test-app`): create 2560×1600 @2× (1280×800 pt), resize to 2880×1800 @2× in place, Retina→1× correctly refused (recreate path), remove, then a second display in the same process works.
- ✅ Live session with Display = Virtual (`probe … 1512x982 --resize-demo 1280x800@100 DIR`): first frame 1.15 s after connect (including display creation) showing the virtual desktop (wallpaper and menu bar only) at exactly 1512×982. Resize to 1280×800 over Display Control: **first new-size frame 172 ms** after the request (target ≈ 1 s). Both keyframes decode. Virtual display gone after disconnect.
- Findings: `CGDisplayCopyDisplayMode` is unreliable for virtual displays (often nil), so we use `CGDisplayBounds`. Retina→1× doesn't apply in place, so we recreate.
- [ ] Human: headless Mac mini (no monitor, no dummy plug) with Display = Automatic. Connect from a 14"/16" MacBook (Windows App, Retina option on) and expect a crisp native-resolution desktop. Resize the client window and expect the desktop to follow within about 1 s.
- [ ] Human: `make spike-vd` on the Mac mini (Phase 0 item).

## Phase 5 — polished app
Automated: 44 Swift tests (audit log, rotation, password policy, settings, password change, diagnostics without secrets) and Rust security end-to-end tests (lockout before TLS, live password change, host disconnect).
Live, 2026-10-21:
- ✅ Connection log on disk recorded a failed login, then connected and disconnected, with time and address. Wording was improved after this run.
- ✅ (Phase 0) `make package` produced `dist/ClawTank-0.1.0.dmg` (9.8 MB, app plus /Applications link) with a .sha256.
- ⚠️ Found: with the Mac locked, clients see only the wallpaper (ADR-018). The menu now says so.
- [ ] Human, the acceptance test: on a fresh macOS user account, open the DMG, drag to Applications, open (Open Anyway), follow setup (Screen Recording, Accessibility, password), then connect from Windows App. Target: under 2 minutes.
- [ ] Human: Settings window tabs, the LAN-only warning dialog, Start at login (may need approval in Login Items), Export Diagnostics, Disconnect from the menu, VoiceOver labels.

## Phase 6 — audio and clipboard
Automated: a Rust end-to-end test with IronRDP's RDPSND and CLIPRDR client channels (48 kHz stereo PCM arrives intact; text both ways), plus Swift tests for text, CF_HTML, DIB (24/32-bit, zero alpha), PCM conversion, silence detection and pasteboard serving.
Live, 2026-10-21 (`probe … --audio 5` while `afplay Ping.aiff` played):
- ✅ System audio reached the client: 48 kHz stereo 16-bit, peak 9716 during the ping.
- ✅ Found and fixed: silence was streamed at 1.5 Mbit/s. Pure digital silence is now skipped, so it measures **0 kbit/s**.
- [ ] Human (Windows App / mstsc): YouTube audio plays in sync on the client; "Mute this Mac while connected" silences the Mac's speakers but not the client.
- [ ] Human: copy and paste both ways for plain text, rich text (TextEdit ↔ WordPad/Word), HTML (Safari ↔ Edge), and images (Preview ↔ Paint).

## Phase 7 — file and folder copy
Automated: Rust end-to-end test of files both ways (folders, size query, byte range); an ignored core-only throughput test (`cargo test --release --test file_throughput -- --ignored`) gave **1.2 ms per 1 MiB request, about 811 MB/s**. Swift tests cover path sanitizing (traversal rejected), folder flattening and range serving.
Live, 2026-10-21, loopback, `files_demo` (examples/files_demo.rs) acting as the client:
- ✅ **Client → Mac, 1 GiB folder** (2 × 512 MiB): 4.9 s (207 MB/s). SHA-256 of both files on disk matched what the client served.
- ✅ **Mac → client, 1 GiB folder** (nested folder plus 3 files, copied in Finder via the clipboard): 4.2 s (244 MB/s). All SHA-256 hashes matched the source.
- Found and fixed: relative paths doubled under symlinked folders. File requests are now served off the main thread. (An apparent 23 ms per request turned out to be the demo client's own loop.)
- [ ] Human: mstsc / Windows App: copy a folder in Finder and paste it in Explorer, and the reverse (it lands in ~/Downloads/ClawTank and is put on the clipboard). Large copy on Wi-Fi.

## Phase 7 — quality
- Adaptive bitrate: policy unit tests, plus an end-to-end check that the client acks frames and latency is measured. [ ] Human: Wi-Fi session, then move away from the router; the menu should show "(congested)" and a lower target, then recover.
- AVC444 measured, no-go (ADR-021): 4:2:0 colour error on small coloured text is 13.5/255 at 1× and 8.5 at 2×, independent of bit rate.
- Multi-monitor deferred (ADR-019).

## Phase 8 — remote access
- Unit tests: `tailscale status --json` parsing (Running, NeedsLogin, Stopped, no MagicDNS, unreadable), detection fallbacks (CLI missing → interface scan → not installed), the 100.64/10 range, `pmset` womp parsing, and large CLI output not stalling.
- Already covered by Rust security tests: 100.64/10 and ULA accepted; public addresses rejected by default.
- [ ] Human: install Tailscale on the Mac and on a phone or laptop. Turn off the client's Wi-Fi (use cellular), connect to the `*.ts.net` name shown in Settings → Remote Access, and confirm a session works.

## Phase 9 — Agent API and MCP
- Swift unit tests:
  - HTTP parsing (incremental, oversize, chunked refused) and a real loopback request
  - action and key-combo parsing, and the token store (hash only, 0600, revoke, reload)
  - the router with a fake host: token required, throttling, Origin and Host refusal, every action reported for the audit log, human-wins 409 for 3 s with screenshots still allowed, error codes
  - app lookup by name and bundle id, with paths refused
- Rust (`clawtank-mcp`): MCP initialize and version negotiation, tools/list, screenshot → image content with screenshot-pixel → point conversion, API errors → tool errors, loopback-only URLs.
- Live acceptance (2026-10-21, MacBook, 1470×956 pt @2x), with a script acting as the MCP client:
  - `clawtank-mcp` handled initialize (2025-06-18), tools/list (12 tools) and list_displays.
  - `screenshot` returned a 1470×956 PNG of the real desktop, with the "Agent … is in control" badge visible.
  - `open_app TextEdit` worked, and `get_state` then reported TextEdit frontmost with its window title.
  - The audit log has one `agentAction` line per step.
  - A bad token got 401 and was logged as `agentRefused`; an `Origin` header got 403.
  - The API listened on 127.0.0.1:3390 only, and refused connections once turned off. ✅

## Phase 10 — release
- `make release` produces `dist/ClawTank-0.9.0-arm64.dmg` (10.3 MB), `SHA256SUMS` and `RELEASE_NOTES.md`. The script refuses mismatched versions or a missing changelog section, and verifies the DMG (hdiutil verify, read-only mount, layout, `codesign --verify --deep --strict`, bundled `clawtank-mcp`).
- Release builds go to `build/release` (ad-hoc), so the dev-signed app keeps its TCC grants.
- [ ] Human: on another Mac, or a fresh user account, a person who hasn't seen ClawTank installs from the README only (download → Open Anyway → setup → connect from a client). Note any step that needed help.
- [ ] Human: record the demo GIF (docs/media/demo.gif) and enable it in the README.
- [ ] Untested: Developer ID signing and notarization path (needs an Apple Developer account).

## MCP Bundle
- `scripts/package-mcpb.sh` checks that the bundled server starts and lists its tools. 2026-10-23: the 0.9.0 bundle lists all 12 tools and passes `mcpb validate` and `mcp-publisher validate`.
- [ ] Human: double-click the `.mcpb` with Claude Desktop installed, enter a token, and ask Claude for a screenshot of the Mac.
