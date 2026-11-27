# macOS 26 Intel + NVIDIA 方向

本分支面向 Intel Mac、macOS 26 和 NVIDIA 显卡上的 ClawTank。工作分为三条
独立验证线：x86_64 构建移植、macOS 运行时兼容性，以及 RDP 性能。构建通过
不能证明 NVIDIA 硬件编码路径可用。

## 分支约定

- `main` 保持为上游基线的本地镜像。
- `macos26-intel-nvidia-rdp` 是个人集成分支，以上游 `upstream/main` 作为
  rebase 基线。
- 功能开发使用短生命周期的主题分支或聚焦提交，每个提交只处理一个问题。
  不提交生成的 Xcode 工程、构建产物或本地签名材料。
- 在发起 Pull Request 或合并上游更新前执行：

  ```sh
  git fetch upstream
  git rebase upstream/main
  make lint test
  ```

- 协议修改、macOS 集成修改和性能调优应尽量拆成可独立审查的提交，便于
  rebase 和向上游移植。

## 当前基线

仓库由 Rust RDP 核心和 Swift macOS 主机构成。主机视频链路是
ScreenCaptureKit → VideoToolbox → RDP EGFX AVC420。ClawTank 没有直接的
NVENC/CUDA 后端；`H264Encoder` 请求 VideoToolbox 提供硬件编码器。社区驱动
项目 NullMoth 将 x86_64 macOS 26 与 NVIDIA Turing 及更新显卡列为目标，其
2.9 parity audit 声称在列出的 GPU 代际上支持通过 VideoToolbox 进行 H.264/HEVC
硬件编码。这说明现有编码 API 可能是可行路线，但仍必须使用 ClawTank 的实际
编码参数和 RDP 链路验证。该 audit 是项目自身报告，不能证明 ClawTank 兼容；
其中 media-service 选择还需要第二台设备复测。

Apple 官方支持的 macOS 26 Intel 机型范围有限，包括 2019 年 16 英寸
MacBook Pro、2020 年四雷电口 13 英寸 MacBook Pro、2020 年 27 英寸 iMac
和 2019 年 Mac Pro，参见 [Apple 兼容性列表](https://support.apple.com/en-us/122867)。
NullMoth 是第三方 OpenCore 驱动项目，不是 Apple 或 NVIDIA 支持的配置；其
安装需要内核扩展以及非默认的 OpenCore 启动和安全设置。每次测试都要记录
GPU 型号、驱动版本和启动配置。其插件、内核扩展和打包组件使用
PolyForm Noncommercial 1.0.0（translator 使用独立的 LGPL-3.0 许可），
不得把这些组件复制或重新分发到 ClawTank 中。

## 本机环境与初步探测

本机探测日期为 2026-11-23：Mac Pro（MacPro7,1）、Intel Xeon W、32 GB 内存、
NVIDIA GeForce RTX 5060（8 GB），系统为 macOS 26.7.3，build `25G342`。
主机架构为 `x86_64`；Swift 6.3.3 的目标为 `x86_64-apple-macosx26.0`，
已安装 macOS SDK 为 26.5。系统能够识别该 GPU，并显示 Metal 4。不同系统接口
报告的 CPU 核心数量不一致，因此这里不记录核心数。

Rust stable 通过官方 rustup 安装并设为默认，版本为 1.100.0，目标主机为
`x86_64-apple-darwin`；`clippy` 和 `rustfmt` 组件也已安装。MacPorts 2.12.7
提供 XcodeGen 2.48.0 和 SwiftLint 0.63.2；Rust 命令验证时将
`~/.cargo/bin` 放在 PATH 前端，避免使用 MacPorts 的 Rust。

本机已安装完整 Xcode 26.6（Build `17F113`），当前开发者目录为
`/Applications/Xcode.app/Contents/Developer`，`xcodebuild` 可用。实际 SDK 为
macOS 26.5。首次通过 MacPorts 编译安装 XcodeGen 后，Intel 构建、测试和发布
流程均已跑通，不再受缺少 Xcode 或 XcodeGen 阻断。

2026-11-23 的 x86_64 验证结果：`make ARCH=x86_64 build` 成功，Release App
主程序和内置 `clawtank-mcp` 均由 `lipo -archs` 确认为 `x86_64`；`make
ARCH=x86_64 test` 成功，Rust 测试 36 项通过、1 项性能测试忽略，XCTest 80 项
通过、3 项跳过；`make lint` 中 Clippy、rustfmt 和 SwiftLint 均通过。完整的
`make ARCH=x86_64 release` 也成功，生成了通过 DMG 完整性、签名、布局和架构
检查的 Intel DMG，以及可启动并列出 12 个工具的 x86_64 MCP Bundle。该构建为
ad-hoc 签名，未进行 Developer ID 签名或公证。

`.github/workflows/ci.yml` 已增加独立的 `macos-26-intel` job：确认 runner
架构为 x86_64，执行 lint、Rust/XCTest，并构建和上传 unsigned Intel release
产物。GitHub 官方托管 runner 文档列有此 Intel 标签；该 job 尚未在 GitHub
Actions 实际运行，仍需首次远端结果确认。参见 GitHub 的
[托管 runner 文档](https://docs.github.com/en/actions/how-tos/write-workflows/choose-where-workflows-run/choose-the-runner-for-a-job)。

本机是 Intel，因此默认构建会自然选择 `x86_64`。Makefile 现在根据 `ARCH`
计算 Rust target，并把同一个 target 传给 Cargo 和 XcodeGen 生成的工程；
`make build ARCH=x86_64` 会使用 `x86_64-apple-darwin`，Apple Silicon 交叉
构建则需要先用 rustup 安装该 target。通用 App 目标没有发现 arm64-only 源码
限制。MCP Bundle 打包脚本和 release helper 会读取 App 与 MCP 二进制的实际
架构，支持 `arm64`、`x86_64` 和 universal，并拒绝两者不一致的产物。

## 当前已确认的初步问题

2026-11-24 按当前工作区验证：`make ARCH=x86_64 lint`、`make ARCH=x86_64 test`
和 `make ARCH=x86_64 build` 均通过。Rust 测试中 37 项通过、1 项性能测试
忽略；XCTest 83 项通过、3 项跳过。Release App 主程序和内置 `clawtank-mcp`
均由 `lipo -archs` 确认为 `x86_64`。这只验证本机 x86_64 构建和测试；没有
替代 Windows 客户端互操作或目标 NVIDIA 驱动下的运行时验证。

当前 Rust 协议栈已从 IronRDP server 0.13/EGFX 0.3 迁移到同一个固定上游
revision `475e5d534f0afa080e51b940caf3d7c1f9d6ec6c`：server 0.14、EGFX 0.5、
core 0.3.1、PDU 0.11，并使用匹配的 client/channel crates。用户提及的
[`ironrdp 0.17.0`](https://docs.rs/crate/ironrdp/0.17.0) 是便捷 re-export 的
meta crate，不能作为 server 组件版本；本项目不依赖它。2026-11-24 核对时，
官方 master HEAD 正是[当前固定提交](https://github.com/Devolutions/IronRDP/commit/475e5d534f0afa080e51b940caf3d7c1f9d6ec6c)。
旧 `core/vendor/` 内容已不进入当前 Cargo 依赖图。

2026-11-24 的 `cargo update --dry-run --verbose` 没有发现可更新的锁定包。它
列出 5 个依赖仍低于 registry 可用版本：`crypto-mac 0.11.0`、
`generic-array 0.14.7`、`picky 7.0.0-rc.27`、`pkcs1 0.8.0-rc.4` 和
`rsa 0.10.0-rc.18`；依赖树显示这些包由当前 IronRDP/SSPI 版本引入并约束，
强行覆盖会破坏兼容性，应随其上游 manifest 更新。

2026-11-24 使用 RustSec 官方 advisory-db 快照运行 `cargo audit`：扫描 387 个
锁定依赖，没有发现 `vulnerabilities` 类告警；发现 11 条 informational warning：
`atomic-polyfill 1.0.3` 和 `pin-utils 0.1.1` 已停止维护，`heapless 0.7.17`
对应 9 条 soundness/未定义行为公告，修复版本为 `heapless >=0.9.4`。当前
`heapless` 路径是 `IronRDP connector → sspi/scard → winscard → iso7816`；在
固定的官方 IronRDP revision 中，connector 明确启用 `sspi` 的 `scard` feature，
而 `iso7816 0.1.4` 仍要求 `heapless 0.7`。ClawTank 不使用智能卡认证，但不能
仅在应用侧关闭此传递 feature，也不能把 `heapless` 强行替换成 0.9.4，因为
其类型/API 与 `iso7816 0.1` 不兼容。修复应在 IronRDP/SSPI 上游移除非必要的
feature，或升级 `iso7816` 及其依赖后再更新本项目的固定协议栈。审计还忽略
了既有 ADR-008 记录的 `RUSTSEC-2023-0071`；重新运行完整审计时需使用最新
RustSec 数据库。

1. `release.sh` 原先规范化了 universal MCP 架构，却没有规范化 universal App
   架构，导致两者比较必然失败；现已统一后再比较。
2. 本机 x86_64 构建、Rust 与 Swift 测试、lint、DMG 和 MCP Bundle 发布检查均
   已通过。FreeRDP 互操作冒烟、真实显示器权限流程、macOS 26 睡眠/唤醒，以及
   NVIDIA VideoToolbox 硬件编码选择和持续 RDP 视频仍需单独验证；编译成功不
   代表这些运行时目标已达成。
3. 诊断包曾显示辅助功能、屏幕录制和 ScreenCaptureKit 音频均被拒绝。根因是
   `/Applications/ClawTank.app` 与 Xcode Debug 产物虽然 Bundle ID 相同，但
   都是 ad-hoc 签名且 CDHash 不同；TCC 授权的是旧副本时，当前进程会继续返回
   未授权。诊断导出现在记录运行路径、Bundle ID、架构、CDHash、签名类型和两项
   preflight 结果。开发构建应先执行 `make dev-signing`，并固定从同一份 App
   运行；切换副本后需要在系统设置中删除旧项并重新授权。

4. 2026-11-23 最新诊断包显示屏幕录制和辅助功能均为 `granted`，两次 RDP 会话
   均完成认证、2316×1080 分辨率协商、音频和 EGFX 协商。失败发生在创建 H.264
   编码会话时，原错误显示为 `EncoderError错误0`。同一主机上的 VideoToolbox
   探针在 2320×1088 等尺寸复现 `EnableLowLatencyRateControl=true` 返回
   `kVTParameterErr (-12902)`；移除此选项后，会话由
   `com.nvmtl.videoencoder.avc.nvenc` 创建，且
   `UsingHardwareAcceleratedVideoEncoder=true`。修复只在该参数错误时删除低延迟
   specification 后重试，并保留硬件必需约束。诊断包中的 `Broken pipe` 与 TLS EOF
   出现在客户端断开之后，是视频启动失败的次生现象。编码器 API 探针已证明硬件
   会话可创建；完整 RDP 连续视频仍需使用新构建和客户端复测。

5. 2026-11-24 的 Android `10.10.10.49` 日志对应服务端 session 23–26：NLA 成功后，
   服务端按客户端提供的 2316×1080 尺寸发送首张传统 BitmapUpdate，编码耗时约
   17–53 ms；客户端随后约 0.07–0.15 秒复位连接。Android 日志四次都报告
   `rowDelta in TS_CD_HEADER incorrect [got 64 expected 256]`，调用栈位于
   `BD_DecompressBitmap`/`ProcessBitmap`，随后以 security disconnect 结束。这将故障
   定位在首张传统压缩位图解码阶段。Android 错误中的 64/256 与 `cbScanWidth` 写入
   像素数/字节数差异吻合。MS-RDPBCGR 文字将该字段描述为像素宽度，但 xrdp 的 32bpp
   RDP 6.0 路径和 FreeRDP 3.33.1 服务端均按行字节数填写；64 像素 tile 对应 256。
   当前编码器已在通用 32bpp 路径采用该互操作约定，tile 几何和 `width` 仍是像素值，
   并增加真实序列化头与长度断言。规范文字和既有实现的差异记录在
   [Windows App RDP 研究](research/windows-app-rdp.md)。仍需独立 FreeRDP 解码测试和
   Android 实机复测；在复测前不宣称问题已解决。
   已启用 IronRDP 的 NSCodec 编码 feature，但只有客户端在 bitmap codec capability 中
   声明 NSCodec 才会协商使用；这不能单独解释或宣称修复 Android 的当前故障。PC 已确认
   AVC420 协商成功，Android 断开前没有对应证据；不能把 EGFX 通道附加等同于能力确认。
   2026-11-25 最新服务端会话进一步确认 Android 的最终 EGFX 能力为 `V10.4` 且
   `AVC_DISABLED`，同时允许 RemoteFX Progressive；客户端未声明经典 RemoteFX bitmap
   能力。当前实现因此在该路径选择 EGFX `WireToSurface2` Progressive，不再默认回退到
   每帧约 30 MB 的传统位图。Rust 集成测试使用同一锁定 IronRDP 客户端实际解码并合成
   Progressive tile、核对像素并收到 ACK；Android 真机复测仍需单独完成，测试通过不代表
   该客户端已接受 Progressive。
6. PC 会话日志确认 `SuppressOutput` 和 `RefreshRectangle` 按预期到达；session 32 的
   9727 帧均获 ACK、无丢帧，但 ACK 只证明 EGFX 管线收到确认，不证明客户端窗口已完成
   呈现。此前 ClawTank 没有读取 IronRDP 每连接的抑制状态，ScreenCaptureKit 静止时也
   不会再交付 `.complete` 样本，所以窗口恢复后不能保证生成新的 IDR。现已读取
   `DisplayContext.display_suppressed` 对应的每连接原子状态，并在 20 ms 轮询内同步到视频
   提交门控；抑制时即使 EGFX surface 尚未建立也不提交画面。恢复后 Swift 编码队列从最近
   缓存的 IOSurface 像素帧请求 VideoToolbox IDR，等待期间有界重试。此处仍需 Windows App
   实机验证；x86_64 编译不能证明客户端实际呈现，也不能证明 NullMoth/NVIDIA 的运行时行为。

7. 2026-11-26 的 Android 握手失败对应的 peer 为 `10.0.0.195`。`01:08:10`、
   `01:08:14`、`01:36:40`、`01:36:42` 和 `01:36:51` 的会话均已完成 CredSSP
   认证（`credential candidate matched`）和 `2316×1080` Client Info 交换；服务端发出
   `358` 字节 `ServerDemandActive` 后收到 `66` 字节帧，并在
   `CapabilitiesWaitConfirm` 报 `failed to accept client during finalize`。该包不是
   TLS 或密码错误；相邻重试收到 `520` 字节 `ClientConfirmActive` 后成功进入
   `ConnectionFinalization`。根因归为确认阶段前置 PDU 的次序竞态，而不是 EGFX、
   Progressive 或 VideoToolbox。

   当前通过本地 `ironrdp-acceptor` patch 复用上游重激活阶段的输入暂存机制：首次连接
   `CapabilitiesWaitConfirm` 现在使用 `RDP_HINT` 接收 Fast-Path，并保留合法的
   Fast-Path、I/O 输入和静态通道帧，在 finalization 后按原始字节顺序回放。已解析但不属于
   这些类别的 I/O PDU 会以带 PDU 类型的错误拒绝，而不是静默丢弃。同一阶段还会把
   `DecodeError` 的完整错误链写入审计详情；认证失败按嵌套
   `sspi::ErrorKind::LogonDenied`/`NoCredentials` 单独识别，TLS、能力确认和 finalization
   失败不再累计密码锁定次数。acceptor 的内部测试覆盖慢路径输入、Fast-Path、静态通道、
   非输入拒绝和嵌套认证错误；完整 `make ARCH=x86_64 lint/test/build` 已通过。

   2026-11-26 12:22–12:23 的实机复测已使用该构建完成验证：服务端在
   `CapabilitiesWaitConfirm hint=RdpHint` 后收到 `520` 字节 `ClientConfirmActive`，
   成功进入 `ConnectionFinalization`；随后协商 `V10_6`/`V10_7` AVC420，
   `com.nvmtl.videoencoder.avc.nvenc` 创建成功，首帧约 185 KB，EGFX
   FrameAcknowledge 连续到达。新日志中没有再出现 `PDU received length=66` 或
   `failed to accept client during finalize`。因此确认阶段握手竞态已由实机复测关闭。
   少数客户端连接的 CredSSP 响应写出前取消会显示 `Broken pipe`，已被归类为客户端
   主动关闭，不计为密码错误和锁定。Android 单独声明 `AVC_DISABLED` 且只选择 RemoteFX
   Progressive 的能力组合仍需在客户端切回该配置后单独验证；AVC420 成功不能替代该条证据。

   2026-11-26 对照了微软当前的 Windows App 发布记录和编码文档：Windows 公共版为
   `2.0.1601.0`、Insider 为 `2.0.1744.0`，macOS GA 为 `11.4.4 (3132)`。官方确认
   Windows 客户端支持 AVC/H.264 与 HEVC/H.265 硬件解码，但 HEVC、AVC444 和
   `Codecs Used` 主要属于 AVD/Windows 365/Dev Box 的 Direct Stream 文档，并未公开
   Windows App 产品版本到 EGFX `AVC420`/`AVC444`/Progressive 的逐版本矩阵。因此实现
   继续只服从最终 RDPGFX capability set：允许 AVC 时发送已验证的 AVC420，
   `AVC_DISABLED` 且允许 Progressive 时发送 `WireToSurface2`，两者都不可用时使用
   legacy bitmap；不按客户端版本或平台分支。具体一手资料和兼容边界见
   [Windows App RDP 研究](research/windows-app-rdp.md)。

8. 2026-11-26 启用 IronRDP 的可靠 UDP 多传输。RDP 会话仍从 TCP 3389 开始，
   同时由同一端口的 UDP socket 提供 RDPEUDP2 + TLS + RDPEMT 侧带；服务端公告
   `TRANSPORT_TYPE_UDP_FECR | SOFT_SYNC_TCP_TO_UDP`。侧带建立后只把 EGFX 动态
   通道迁移到 UDP，DRDYNVC 控制、输入、Display Control、音频和剪贴板仍走 TCP。
   客户端不支持、侧带失败或返回 `E_ABORT` 时 EGFX 保持 TCP。迁移完成后 UDP
   侧带丢失会结束连接，不会在通道已经标记为 UDP 后静默继续发送到 TCP。
   当前实现使用可靠 RDPEUDP（FECR）；尚未请求 lossy `UDP_FECL`。

   本机 loopback 集成测试使用真实 RDPEUDP2/TLS/RDPEMT socket 验证了三件事：
   AVC420 在迁移后经 UDP 解码出图形更新且 ACK 返回服务端；迁移后 Display
   Control resize 仍经 TCP 往返；客户端拒绝 UDP 时 AVC420 仍经 TCP 到达。
   这只证明锁定 IronRDP revision 的协议和传输实现，不证明 Windows App、
   FreeRDP、Android 或 iPad 在真实 Wi-Fi、NAT、防火墙和丢包路径上会接受
   UDP。端口转发与防火墙若使用非 Tailscale 路径，必须同时放行同号 TCP 和 UDP。

9. 2026-11-26 增加面向客户端首屏的连接诊断叠加层。连接在 EGFX 能力确认前，
   Rust 先发出初始桌面尺寸事件；Swift 按该尺寸渲染全屏 BGRA 诊断画布并通过窄
   FFI 回填，Rust 在 800 ms 内有界等待，超时仍回退原测试图，不阻塞握手。
   叠加层默认开启，用户关闭后测试图和实时画面都不绘制。它展示认证/RDP 会话、
   分辨率、desktop/device scale、显示器数、EGFX 通道和最终 capability、
   AVC420/AVC444/Progressive/legacy bitmap 能力、经典 bitmap codec 与 surface
   flags、客户端 UDP_FECR/Soft-Sync 公告和 Multitransport 响应，以及当前
   TCP/UDP 路径。字段来自结构化 CoreEvent，不依赖解析日志；平台和客户端产品名
   仍标记为未知，不能从这些协议自报字段推断。首个真实 AVC/Progressive 帧已进入
   core 会话队列后，全屏测试图布局切换为左上角紧凑角标；legacy bitmap 路径没有
   ACK，不能把“已提交”冒充“客户端已呈现”。真实 Windows App、Android 和 iPad
   的视觉切换仍需实机复测。

## RDP 客户端显示信息与 HiDPI

`MS-RDPEDISP` 的 `DISPLAYCONTROL_MONITOR_LAYOUT` 包含桌面像素宽高、相对主屏的
位置、物理宽高（毫米）、方向、桌面缩放因子和设备缩放因子。物理尺寸和方向是
客户端提供的显示器描述；物理尺寸无效时（任一边小于 10 mm 或大于 10000 mm）
必须忽略。它们不是设备型号、GPU 信息或面板原生像素分辨率。可见的远程像素
尺寸应以客户端请求的 `Width`/`Height` 为准，不能从毫米数推算原生分辨率。

当前服务器按客户端请求的像素宽高创建/调整桌面；“跟随客户端 DPI”只把合法的
桌面与设备缩放因子作为 HiDPI/标准密度提示，用户仍可选 1× 或 2×。本轮增加的
Rust 诊断会记录监视器数量、主屏标记、像素尺寸、位置、有效物理尺寸、方向与两项
缩放值，以验证 Windows App 实际发送的内容。没有这些字段时维持现有回退策略。

单屏尺寸上限采用每边最多 3840 像素、总面积最多 `3840×2400` 像素：宽高轴向限制
对称，避免把合法的 `1440×2560` 竖屏高度裁到 2400；面积预算与 IronRDP 对外公布的
单屏 Display Control 能力一致。初始桌面协商不支持只改服务端 framebuffer 尺寸，
所以超面积初始请求在能力协商前拒绝；动态超限布局被忽略并保留当前布局。Swift 虚拟
显示再执行同一边界检查，`CGVirtualDisplay` 的最大宽高固定为 3840×3840，不根据
客户端请求扩大。面积上限避免竖屏修正同时增大既有 framebuffer 预算。

后续若增加“按客户端面板尺寸设置虚拟显示器物理尺寸”的选项，只能把有效的
`PhysicalWidth`/`PhysicalHeight` 当成 macOS `CGVirtualDisplay` 的可选物理尺寸提示，
并提供当前 110/220 PPI 估算作为回退；不能用它改写 RDP 桌面像素尺寸或推断原生
分辨率。Windows App 是否上报有效值、系统是否按预期应用该 metadata，以及真实
虚拟显示 HiDPI 是否生效，都必须由 Windows App 真机连接与 macOS 26 实机日志验证。

RDP 握手没有一个可靠字段声明客户端产品名或操作系统设备型号。客户端名、版本、
能力和显示布局只能作为协议自报信息记录，不能据此断言连接来自 Windows App；
当前“client platform unknown”是准确状态。RDP/EGFX 传输和 NVIDIA VideoToolbox
编码属于不同层，客户端显示 DPI 不应改变 Rust 协议核心的 GPU 厂商策略。

## xrdp 设计参考

只参考 neutrinolabs/xrdp 的公开结构设计，未移植其 C 代码。核对版本为
`devel` commit `7a7a112758bef815b08fcf5ea750a9c5ac1c34d`，重点文件是
[`common/xrdp_client_info.h`](https://github.com/neutrinolabs/xrdp/blob/7a7a112758bef815b08fcf5ea750a9c5ac1c34d/common/xrdp_client_info.h)、
[`libxrdp/libxrdp.c`](https://github.com/neutrinolabs/xrdp/blob/7a7a112758bef815b08fcf5ea750a9c5ac1c34d/libxrdp/libxrdp.c)
和
[`xrdp/xrdp_mm.c`](https://github.com/neutrinolabs/xrdp/blob/7a7a112758bef815b08fcf5ea750a9c5ac1c34d/xrdp/xrdp_mm.c)。

- xrdp 把当前客户端 monitor layout 和 resize 状态放在每连接状态中；ClawTank 当前
  是单活连接，因此继续用单一 `VideoHub`，但在每次会话结束时重置像素尺寸与 DPI，
  不让后续连接继承上一次布局。
- xrdp 先解析并校验完整的 monitor layout，再排入会话 resize 队列；队尾尚未开始的
  连续客户端 resize 可以合并为最新请求，正在执行的 resize 仍串行完成。ClawTank
  由 IronRDP 解析 PDU，Rust 校验单屏像素尺寸与面积，Swift 仅合并视频队列中尚未开始
  的最新请求；正在执行的显示/编码生命周期与断连清理仍按序运行。
- xrdp 将 resize 拆成输出抑制、图形表面重置、后端显示调整、布局提交和恢复输出等
  阶段，并记录处理状态与时长。ClawTank 保留已有的 EGFX `ResetGraphics`、旧尺寸帧
  拒收和恢复 IDR 路径；macOS 虚拟显示仍通过 `VirtualDisplayProvider` 管理，不模拟
  xrdp 的 Xorg/VNC module 或桌面服务器回调。
- xrdp 的布局相等判断没有比较 orientation 和物理尺寸。ClawTank 会继续诊断这些值，
  但仅在其变化影响本项目实际消费的尺寸或 DPI 状态时重启显示管线；当前不按方向二次
  交换宽高，也不从面板毫米数推算像素分辨率。

这项布局工作与传统位图编码路径相互独立。针对 Android `0x510d` 的修正只改变
32bpp RDP 6.0 压缩头的 `cbScanWidth` 字节/行值，不改变客户端平台识别或 EGFX 路径；
仍需独立解码器与 Android 真机日志验证结果。

## 验收矩阵

1. **构建：** 本机完整 Xcode 26.6、rustup Rust stable、XcodeGen 和 SwiftLint
   已就绪；x86_64 App 与内置 MCP 构建及发布检查通过，二进制架构均为
   `x86_64`。Apple Silicon 交叉构建需先用 rustup 安装相应 Rust target。
2. **测试：** `make lint` 和 `make test ARCH=x86_64` 通过。FreeRDP 互操作冒烟
   尚未执行；需要真实显示器或系统权限的场景单独记录为手动结果。
3. **macOS 26 运行时：** Screen Recording 和 Accessibility 权限正常，虚拟
   显示器创建与调整大小正常，音频/剪贴板/输入正常，并能经受睡眠、唤醒和
   显示器变化。
4. **视频：** 记录 VideoToolbox 编码器标识、硬件加速结果、帧率、编码延迟、
   码率和 CPU/GPU 负载；同时测试物理显示器和虚拟显示器。
5. **RDP 互操作：** 使用 Windows App 或 `mstsc`、FreeRDP 3 和第二个 macOS
   客户端验证 NLA、调整大小、AVC420、光标、输入、音频和剪贴板。每项结果
   记录客户端、GPU、驱动和 macOS build。

编码器证据必须查询 VideoToolbox 编码器状态，记录编码器标识和
`UsingHardwareAcceleratedVideoEncoder` 结果。Apple 的
[VideoToolbox 文档](https://developer.apple.com/documentation/videotoolbox/kvtcompressionpropertykey_usinghardwareacceleratedvideoencoder)
将该属性定义为运行时判断硬件编码是否实际启用的信号。不能因为显示加速就
推断 NVIDIA 编码器已被选中；必须在目标 Tahoe build、GPU 和驱动组合上测试。

## 工程边界

- 保持 Rust/Swift 边界：Rust 负责 RDP，Swift 负责 macOS API。
- NVIDIA 相关逻辑放在小型视频编码抽象后面，不让 RDP 协议依赖厂商 API。
- 如果 VideoToolbox 在 Intel/NVIDIA 系统上无法提供硬件编码，先加入明确的
  检测、诊断和经过测试的回退策略，不得静默宣称使用硬件加速。
- `CGVirtualDisplay` 继续隔离在 `VirtualDisplayProvider` 后面；macOS 26
  行为需要真实机器验证，因为它使用私有 API。
- 遵守 `CONTRIBUTING.md` 的互操作分析与复用边界：可检查经授权的闭源客户端
  工件以研究互操作行为，但不得复制其专有代码、资源、界面文案或品牌内容；
  将工件观察与公开规范要求分开记录，并独立验证线上协议行为。

## Rust UI 可行性评估

Rust GUI 可以替换设置、首次运行、连接说明和日志等窗口的 SwiftUI 绘制层。
当前最适合做原型的是 `egui/eframe + wgpu`：它可以在 macOS 使用 Metal，并可
接入 AccessKit；本项目 MSRV 已提升至 Rust 1.94，采用前仍需验证 Intel 上的
窗口、VoiceOver、深色模式和密码输入。
`wgpu` 是绘制后端，不是 macOS 系统能力的替代品，也不会自动启用 NVIDIA 视频
编码。

完全移除 Swift 仍不能移除 Apple API：ScreenCaptureKit、VideoToolbox、CGEvent
与 Accessibility、私有 `CGVirtualDisplay`、NSPasteboard、Keychain、登录项、
菜单栏和原生窗口都需要通过 Rust 绑定或一层 Objective-C/Swift 适配层接入。
因此本分支先保留 Swift macOS 宿主，只把 Rust UI 作为独立实验目标；稳定后再
分阶段迁移。若决定全面迁移，必须先更新 ADR-001/ADR-002 以及本项目的 Rust/Swift
职责约定，不能把 UI 迁移和 RDP、x86_64 构建或 NVIDIA 编码改动混在一个提交中。

## 后续实施顺序

1. 已完成本机 x86_64 构建、测试、lint 和发布打包验证，并增加专用 Intel CI
   工作；待首次 GitHub Actions 运行确认远端 runner 结果。
2. 在目标 GPU 上测试现有 VideoToolbox 硬件必需路径，检查编码器选择和持续
   RDP 输出；只有测量显示现有路径达不到目标时，才增加编码器回退抽象。
3. 只修复已测量到的 RDP 回归，并让每个协议或主机改动保持独立、可审查、可
   向上游移植。
