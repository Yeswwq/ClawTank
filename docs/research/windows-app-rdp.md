# Windows App 与 RDP 协议诊断

核对日期：2026-11-26。Windows App 版本和能力信息来自本轮 Microsoft Learn
与 Microsoft Store 的联网核对；客户端实际能力仍以每条连接的最终 EGFX
`CapabilitiesConfirm` 为准。

## 产品与连接范围

Microsoft Store 产品 ID `9N1F85V9T8BN` 对应 Windows App。它是跨平台产品家族，
但 Windows、macOS、Android 等版本不是一个可由 RDP 握手互认的平台客户端。
微软的连接矩阵列出 Remote PC 对 Windows、macOS 和 Android 的支持；Windows
版 Windows App 当前不支持 Remote Desktop Services，微软建议此用途继续使用
Windows Remote Desktop 客户端。ClawTank 提供直接连接本机的 PC 场景，应分别
验证各平台 Windows App 的 Remote PC 实现。

## 客户端识别边界

MS-RDPBCGR 的 Client Core Data 包含客户端提供的 `clientBuild`、计算机名
`clientName`、产品标识 `clientDigProductId`、请求分辨率、网络类型提示和早期
能力标志。协议没有将这些字段定义成 Windows、macOS 或 Android 平台身份，也没有
认证其真实性或定义通用的产品标识到平台映射。`clientName` 是设备名，不是应用
名称；网络类型是客户端提示，不能替代测量结果。

因此，ClawTank 不依据这些字段猜测客户端平台或选择专属传输参数。当前锁定的
IronRDP `ConnectionHandler` 可向应用回报 Client Info/性能标志、桌面尺寸和部分
已协商会话摘要，但没有暴露完整 Client Core Data 中的产品 ID、设备名和 build
字段。日志记录回调实际提供且经过筛选的字段，不读取原始 PDU、不记录设备名、
IME、时区、凭证、routing token 或 cookie；平台标为 `unknown`。

## HiDPI 虚拟显示策略

RDP 初始 desktop width/height 和 Display Control monitor dimensions 表示会话桌面的像素
尺寸。`desktopScaleFactor` 是客户端声明的缩放提示，不是设备面板原生像素数，也不能
可靠识别 Windows、macOS 或 Android。ClawTank 将两者分别传给 Swift：像素尺寸保持原值，
缩放提示只决定虚拟显示器是否使用 2× HiDPI 模式。设置提供“尽量遵循客户端 DPI 提示”、
标准 1×和 HiDPI 2×。只有 `desktopScaleFactor` 在 100–500 范围且配套
`deviceScaleFactor` 为 100、140 或 180 时才采用该提示。有效桌面缩放提示在 150% 及以上
时选择 2×，低于 150% 时选 1×；缺失或无效时不依据该提示启用 HiDPI，改用 1×。
这是明确的近似策略，不会声称精确匹配 125%、150% 等系统缩放等级或客户端的物理面板。
2× 模式不能准确表示奇数宽高，因此这些会话自动回退至 1×。RDPEDISP 规定桌面和设备
缩放值应成对校验；若 Display Control 布局省略或提供无效配对，则保留同一连接已收到的
有效初始提示。

此策略只配置 ClawTank 的虚拟显示输出。ScreenCaptureKit 仍按 RDP 请求的 framebuffer
采集，VideoToolbox 仍使用主机上实际可用的硬件编码器；协议核心不依赖 NullMoth、NVIDIA
或专用编码器名称。macOS 26 上的私有 `CGVirtualDisplay` 行为与 RTX/NVMTL 编码稳定性需在
本机真实客户端连接中单独检查，编译和单元测试不能替代该验证。

## 图形通道选择

RDPGFX 客户端先公告能力集，服务端选定并确认双方支持的能力集。后续图形编码
必须服从已确认的能力，而不是根据产品名推测。ClawTank 当前的视频链路使用
VideoToolbox H.264 AVC420，经 EGFX 动态通道发送。若确认能力含 `AVC_DISABLED`，
客户端不支持任何 AVC 模式；若同一能力仍允许 RemoteFX Progressive，则走
`WireToSurface2` 发送 Progressive，否则保留基础 RDP 位图画面。不能继续发送
H.264，也不能把“连接成功”解释为“实时视频协商成功”。

高性能路径需分别记录和验证：EGFX 公告版本与标志、服务端确认的能力集、AVC420
是否启用、视频启动结果、首帧排队结果、客户端帧确认、队列深度和确认延迟。只有
协议实际协商并打开的传输才可报告为已启用；不能把理论上的 UDP、多传输或客户端
平台支持写成当前已经使用的路径。

### 最新 Windows App 与编码支持

截至 2026-11-26，Microsoft Learn 列出的当前 Windows App 版本是：

| 客户端 | 当前版本 | 与 ClawTank 的关系 |
|---|---|---|
| Windows | 公共版 `2.0.1601.0`；Insider `2.0.1744.0` | 从 `2.0.1482.0` 起 Remote PC 连接 GA；继续使用标准 RDP Remote PC 路径。 |
| macOS | GA `11.4.4 (3132)` | 支持远程 PC；`11.1.6` 的官方发布记录明确改进了 AVC 解码。 |
| iOS/iPadOS | GA `11.3.9 (5948)` | 支持远程 PC；不能从版本号推断其最终 EGFX 编解码器。 |
| Android/Chrome OS | GA `11.0.26101.14228` | Android 11 月起增加 Entra 认证的 Remote PC preview；直接密码/NLA Remote PC 仍以实机能力确认为准。 |
| Web | GA `2026.0.10051.3` | 浏览器客户端不是 ClawTank 当前的原生 EGFX 验收目标。 |

Microsoft Store 页面在核对时为站点版本 `2026.11.2.188`，产品最近更新于
2026-11-20；这是商店网页和产品更新时间，不是底层 RDP 客户端版本。

微软的一手资料确认 Windows 版 Windows App 支持 AVC/H.264，并支持 HEVC/H.265
硬件解码；Windows 版从 `2.0.503.0` 起还可在需要时安装仅用于 Windows App
远程会话的 HEVC 扩展。macOS `11.1.6` 发布记录明确提到 AVC 解码改进，iOS
`11.1.7` 则修复了 AVC 解码的色度渗色。官方文档
没有公开一个可从 Windows App 产品版本映射到 `AVC420`、`AVC444` 或 RemoteFX
Progressive 的逐版本 RDPGFX 能力矩阵。因此不能仅凭商店版本断言某条连接选择
了哪种 EGFX 编解码器。

官方 GPU 编码文档中的 AVC/H.264、HEVC/H.265 和 `Codecs Used` 诊断主要描述
Azure Virtual Desktop、Windows 365 和 Dev Box 的 Direct Stream，不是对任意
自建 RDP 服务端的 HEVC 或 AVC444 承诺。特别地，HEVC 是另一套 Direct Stream
能力；ClawTank 当前没有实现 EGFX HEVC 发送器，也不能把客户端 HEVC 解码能力
当成可直接协商的 AVC444/EGFX 能力。AVC/H.264 仍是最稳妥的跨平台兼容路径；
官方资料明确 AVC/H.264 在本地设备上广泛可解码。

ClawTank 的兼容策略因此不是按 Windows App 版本或平台分支，而是：

1. 只采用最终确认的 RDPGFX capability set。
2. 客户端允许 AVC 时优先发送已实现且已验证的 AVC420；AVC444 发送器未实现前，
   即使 peer 能力暗示支持也不发送 AVC444。
3. 客户端声明 `AVC_DISABLED` 但允许 Progressive 时，使用 `WireToSurface2`
   RemoteFX Progressive。
4. 两者都不可用时回退 legacy bitmap；不得强行发送 AVC。
5. 记录 peer 公告、服务端确认、选中编解码器、实际主机发送路径、首帧和 ACK。
   日志中 `selected_video_path` 必须与 `actual_video_path` 一致，并由断言和
   完整测试锁定该不变量。

本机 2026-11-26 的 Windows App PC 实机会话各自确认 `V10_6`/`V10_7`，协商
AVC420，编码器为 `com.nvmtl.videoencoder.avc.nvenc`，首帧和后续 FrameAck
均到达。这证明当前 Windows App 的该配置与此实现能互通，但不证明它在所有
配置中都会选择 AVC420，也不能替代 `AVC_DISABLED + Progressive` 的 Android
实机验收。

## 本机 Android 实测

2026-11-24，用户确认 Android Windows App 客户端地址为 `10.10.10.49`。本次
导出的 Android logcat 在 14:39:26、14:47:34、14:47:37 和 14:48:04
（本机时区 UTC+08:00）记录了四次相同的失败。服务器日志按客户端 IP、会话顺序
和断开模式与这些尝试对应；两端时钟存在数百毫秒偏差，不用毫秒级时间差推断因果。

Android 日志明确把失败定位在首批 Fast-Path 位图处理：`rowDelta in TS_CD_HEADER
incorrect [got 64 expected 256]`，随后 `BD_DecompressBitmap failed`、
`ProcessBitmap failed!`、`Disconnect for security`，断开码为 `0x510d`
（`legacyCode=20749`）。这排除了“仅连接/认证失败”，但错误文本中的 `rowDelta`
是客户端解码器内部诊断，现有证据不能将它直接等同于线上 `TS_CD_HEADER` 的某个字段。

四次成功完成认证的会话均请求 `2316×1080`，记录的 `client_info_flags` 为
`5980667`、`performance_flags` 为 `198`、desktop/device scale factor 均为 `100`，
`monitor_count=0`，未请求自动重连。RDP 握手没有经验证的平台标识；Android 归属
来自用户提供的设备和 IP 信息，服务端日志中的 `client_platform="unknown"` 是预期结果。

服务端在这些会话中创建了 EGFX graphics session 并发送测试帧，位图编码耗时记录
在约 41–67 ms；随后服务端读到客户端关闭连接所造成的 TLS EOF/复位。日志没有
出现这些会话的 EGFX capabilities-confirmed 或 AVC420-ready 记录，所以只能确认
EGFX 通道附加，不能确认双方完成了图形能力协商。Android logcat 另有
`PopulateCacCodecCaps failed`，但紧随其后的致命链是位图解压失败；目前不能证明
前者导致断开。权限属性读取拒绝、时区回退和剪贴板告警也没有与这次断开的直接因果证据。

本次定位到 `TS_CD_HEADER.cbScanWidth` 的实际互操作差异。MS-RDPBCGR
§2.2.9.1.1.3.1.2.3 将该字段文字定义为像素宽度，要求可被 4 整除；但 xrdp 的
32bpp `libxrdp_send_bitmap` 以 `(width + padding) * bytes_per_pixel` 写入 line size，
FreeRDP 3.33.1 的 32bpp planar 服务端路径也写 `bitmap->width * 4`。对于 64×64、
32bpp tile，二者都发 `cbScanWidth=256`、`cbUncompressedSize=16384`。Android
Windows App 的解码器恰好报 `got 64 expected 256`，与服务端按像素而非字节写入
扫描宽度完全吻合。

2026-11-26 使用启用可靠 UDP 的新构建复测时，Android `10.10.10.40` 仍未声明
`UDP_FECR`，服务端按预期保持 EGFX 在 TCP；客户端在首张传统 RDP6 位图后立即复位。
随后的代码核对发现诊断版一度按规范文字把 `cbScanWidth` 写成像素值 `64`，与此前
Android `rowDelta got 64 expected 256` 的证据再次吻合。当前编码器已恢复 32bpp
互操作路径的字节行距：完整 64 像素 tile 写 `256`，4 像素边缘 tile 写 `16`；
`cbUncompressedSize` 仍为 `encoded_width * height * bytes_per_pixel`。真实序列化测试
覆盖完整 tile、边缘 tile、压缩头完整性和流长度一致性。

这仍是基于公开规范差异、参考实现和客户端可观察错误的兼容选择。必须用同一 Android
客户端复测首帧是否继续显示 `rowDelta`；同时保留 FreeRDP 独立 planar 解码作为第二
证据。若不同客户端需要不同解释，必须按能力和客户端行为分支，不能用颜色现象反推扫描
宽度，也不能让同一实现同时发送两种无法区分的字段值。

新增的 `nla_client_decodes_android_sized_bitmap_frame` 集成测试以 `2316×1080`、
32 bpp 测试图连接当前 ClawTank 服务端，并由同一锁定 IronRDP revision 的客户端
解码及比对图案，测试通过。这降低了“该尺寸的通用编码路径必然损坏”的可能性，
但编码端和测试客户端属于同一 RDP 实现，不能证明 Android Windows App 兼容，也不能
替代 FreeRDP 的独立解码或 Android 实机复测。当前 Mac 未安装 FreeRDP 命令行解码器。

同日 Windows PC 客户端 `10.10.10.51` 的一次成功会话（14:05:44）请求 `1920×1080`，
`client_info_flags=213435`、`performance_flags=6`。服务端完成 EGFX `V10_7`
能力确认，报告支持 AVC420 并进入视频路径，最终收到正常 Disconnect。这证明 PC
会话可走通该图形路径，但不证明 Android 采用相同能力集、位图解码器或时序。

### 首帧位图元数据复测

诊断观察点位于 IronRDP legacy Bitmap Update 序列化完成后、TLS writer 加密前。诊断
默认关闭；受控启动时设置 `CLAWTANK_RDP_BITMAP_DIAGNOSTICS=1`。解析器只读取固定
头、长度字段和 `TS_CD_HEADER` 的原始 little-endian 数值，将压缩流切片立即丢弃。每个
tile 的结构化事件包含矩形、编码宽高、位深、压缩 flags、`cbCompFirstRowSize`、
`cbCompMainBodySize`、`cbScanWidth`、`cbUncompressedSize`、位图流长度和压缩主体长度；
最多记录 4096 个 tile，并记录是否截断。事件没有像素、压缩流、凭证、cookie 或客户端
设备名。诊断 session ID 在认证后的每连接状态中创建，跨 resize/reactivation 保留，
避免预认证候选连接覆盖活动会话的归属。

复测步骤：

1. 使用同一构建启动诊断版并记录 `runtime-session.txt` 的 `runID`、开始时间和进程路径。
2. Android Windows App 连接一次，记录客户端 IP、客户端错误码和本地时间；服务端以
   `session` 关联 `RDP connection established`、`rdp_first_bitmap_tile` 和
   `rdp_first_bitmap_complete` 事件。
3. 用相同合成格式与尺寸检查结构化字段；把同一服务端生成的合成位图输入独立解码器，
   比较预期像素校验值。不得导出或留存真实桌面数据与压缩流。
4. 在同一服务端二进制上用 Android Windows App 再测一次，确认结果与该 session 的首帧
   元数据对应。记录 app 版本、Android 版本、请求尺寸、错误码和时间。
5. 对照 xrdp/FreeRDP 编码结果和 MS-RDPBCGR 文字记录差异；再由 Android Windows App
   实机连接验证互操作效果。不要按客户端平台分流该字段。

xrdp 的公开实现可参考“压缩与序列化处记录尺寸、位深和长度，明确省略矩形内容”，以及
把客户端协商状态和 resize 生命周期放在每连接 session 中。它在 32bpp 压缩路径中以
字节为单位填写行扫描宽度；FreeRDP 3.33.1 服务端采用相同值。参考固定提交
`7a7a112758bef815b08fcf5ea750a9c5ac1c34d` 的
`libxrdp/libxrdp.c::libxrdp_send_bitmap` 与 `xrdp/xrdp_mm.c`；没有移植其 C 代码。
xrdp 的结构设计用于隔离每连接协商状态，位图字段行为则作为互操作证据；它不能消除
MS-RDPBCGR 文字和主要实现之间的差异。也不对 xrdp 的完整客户端结构体调用 Debug
格式化，因为其含有凭证字段。

## 从 xrdp 提炼的连接与 resize 设计

xrdp 值得借鉴的是会话状态的归属和生命周期，而不是它的 C 结构体或 Xorg 后端。
它把监视器布局放在客户端连接状态中；resize 请求先进入队列，只替换队尾尚未处理的
连续客户端请求，正在执行的状态机仍按序完成。它比较布局时也会检查各屏幕位置、尺寸
和可应用的缩放信息，而不只比较整个桌面的外框。ClawTank 是单屏、单活连接，不需要
照搬 xrdp 的多屏数组和后端状态机；合适的最小边界是一个连接上下文，持有已确认的
EGFX 能力、已验证的像素布局与缩放提示、图形通道和 surface 状态、输出抑制状态、待 IDR
标志，以及该连接自己的帧确认统计。当前 `VideoHub` 承担单活连接的协议状态，Swift
`videoTask` 串行化显示与编码器生命周期，并合并队列中尚未开始的布局请求。连接结束时
上下文失效并清理布局、surface、能力和待确认帧；若以后支持并发会话，再把上下文移为
每连接实例，不以客户端身份选择协议行为。

生命周期需要遵守以下不变量：

- 能力公告只是输入，只有服务端选定并发送 `CapabilitiesConfirm` 后才记录为已协商；
  AVC420 仅由最终确认的 capability set 决定。DVC/EGFX 通道打开本身不表示 AVC420 可用。
- 所有 resize 都先把整份布局解析、边界校验并归一化为一个不可变请求，再增加该连接的
  布局代次。像素宽高是协议尺寸；缩放提示和物理面板信息保持为独立字段，不能用来识别
  平台或改写像素尺寸。当前 core 会校验尺寸并拒绝旧尺寸视频帧；Swift 按序应用显示器和
  编码器变更。显式布局代次贯穿 Rust 事件、Swift 异步采集/编码结果和 ACK 的检查，仍是
  可进一步收紧的设计点；现在不可把它描述为已实现。
- 同一连接中，尚未开始应用的多个布局请求可以合并为最新的有效请求；已进入显示器或
  图形重置阶段的请求必须串行完成。执行中到来的更新排在后续，不得让旧请求覆盖新布局。
  这借鉴 xrdp 的尾部合并规则；ClawTank 无需为单屏连接复刻其多来源请求队列。
- resize 期间暂停旧尺寸帧提交，更新主机显示/采集尺寸，重置 EGFX surface，再以新代次
  接受编码帧并请求 IDR。任何异步采集、编码或 ACK 回调都携带或核对代次；旧代次结果
  丢弃，不能写入新 surface。主机显示调整失败时，不应继续把新尺寸帧送入旧 surface，
  而应保持视频门控关闭并报告失败阶段。
- `SuppressOutput` 与 resize 是不同状态：前者只暂停可见输出，恢复时请求新 IDR；后者
  改变布局并重置图形 surface。两者同时发生时，先完成布局代次切换，待输出恢复后再
  请求该代次的关键帧。

并发边界也应明确。IronRDP 在持有 graphics pipeline 锁时调用 `GraphicsPipelineHandler`；
而发送帧的路径需要先持有 `VideoHub` 会话状态再进入 pipeline。因此回调不能反向获取
会话状态锁，也不能在 pipeline 锁内执行可能重入主机层的同步操作。回调只更新原子状态
或投递轻量事件；涉及 surface、尺寸和主机操作的工作由会话串行路径处理。帧计时表、
会话状态和 pipeline 应各有清晰所有者，避免通过多个互相嵌套的锁拼接一次生命周期。
调整布局时，旧代次帧计时应在代次切换的同一临界区内失效，不能先解锁再清表，否则
可能删除新尺寸帧刚登记的 ACK 时间。

帧 ACK 只说明协议客户端回报了图形帧，不等价于窗口完成呈现。统计表应只保留有界的
在途帧记录，并对 ACK 暂停、surface reset、断连和超时清理有明确语义；暂停期间不能
因为不再收到逐帧 ACK 而无限累积重复的延迟样本。若协议库内部 tracker 也会在暂停时
持续记录帧，应在 IronRDP 的公共 flow-control 边界修正并增加测试，而不是只截断
ClawTank 的统计数字。对外指标应分开标注“已排队”“已确认”和“客户端窗口呈现未知”。

诊断按连接 ID 和布局代次关联阶段：已协商能力、请求/应用尺寸、surface reset、首个
关键帧排队、ACK 暂停/恢复、队列深度、ACK 延迟及 resize 耗时。记录失败阶段和错误类别，
不保存像素、压缩负载、原始握手、凭证或未筛选的客户端信息。这样可以用同一套日志
比较 Windows、macOS、Android 客户端，而不把未经认证的 `clientName`、build 字段或
某次错误码变成协议分支条件。

上述规则应以状态转换测试覆盖，而不只测试单个辅助函数：并发 `on_ready` 与 resize
不能启动过期尺寸；旧代次的采集结果和 ACK 不能修改新会话统计；输出恢复只产生一次
刷新请求；ACK 暂停及长时间无 ACK 时在途记录保持有界；surface reset 后首个接受帧必须
是关键帧。xrdp 的 session-local layout/resize 组织方式为这些边界提供了参考，但协议
正确性仍以 MS-RDP 规范、IronRDP 的 API 契约和跨实现互操作测试为准。

本机诊断命令：

```sh
CLAWTANK_RDP_BITMAP_DIAGNOSTICS=1 make full-run ARCH=x86_64
```

应用“设置 → 通用 → 诊断”可调整 Rust 默认日志级别；默认“标准”保留现有全局
`info` 与核心/RDP 模块 `debug` 过滤。首帧 tile 元数据事件仍需同时启用上面的受控
环境开关；不打开时不会因为选择较详细日志级别而解析或记录位图元数据。复测后可将
级别恢复为“标准”或更低。

FreeRDP 3.33.1 的公开 planar decoder 可作为独立压缩体验证器；目前本机未安装，Rust
集成测试仍由 IronRDP 自身客户端解码，不能替代它。Android 真机复测必须在同一构建上
发起；当前尚无修复后的结果，不宣称 Android 实测已通过。

## 日志范围

详细的协议与性能摘要写入统一诊断日志，连接审计日志只保留连接结果等精简记录。
诊断日志不得包含原始握手包、密码、凭证、自动重连 cookie、routing token、IME
名称、客户端计算机名或完整键盘/时区信息。首个 TCP 接入、认证完成/失败、RDP
Client Info、会话配置、EGFX 能力确认、视频首帧和 ACK 应能按时间及会话顺序关联。
对客户端平台的归因需依赖客户端自身可验证的测试记录，而不是不可信的握手字段。

## Microsoft 一手资料

- [Windows App overview and supported connections](https://learn.microsoft.com/en-us/windows-app/overview)
- [What's new in Windows App](https://learn.microsoft.com/en-us/windows-app/whats-new)
- [Microsoft Store: Windows App](https://apps.microsoft.com/detail/9N1F85V9T8BN)
- [Windows App feature comparison](https://learn.microsoft.com/en-us/windows-app/compare-platforms-features)
- [Display settings in Windows App](https://learn.microsoft.com/en-us/windows-app/display-settings)
- [Graphics encoding over Direct Stream](https://learn.microsoft.com/en-us/azure/virtual-desktop/graphics-encoding)
- [Enable GPU acceleration for Azure Virtual Desktop](https://learn.microsoft.com/en-us/azure/virtual-desktop/enable-gpu-acceleration)
- [MS-RDPBCGR: Client Core Data](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/00f1da4a-ee9c-421a-852f-c19f92343d73)
- [MS-RDPBCGR: Compressed Data Header (TS_CD_HEADER)](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/3d1ccc49-51e2-4a2c-b300-3da9fb931d0e)
- [MS-RDPBCGR: Bitmap Data (TS_BITMAP_DATA)](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/84a3d4d2-5523-4e49-9a48-33952c559485)
- [MS-RDPEGFX: Capabilities Advertise](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/9cc3cf56-148d-44bf-9dea-5f5e6970c00f)
- [MS-RDPEGFX: Capabilities Confirm](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/4d1ced69-49ea-47dd-98d6-4b220f30db36)
- [MS-RDPEGFX: AVC_DISABLED](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/d1899912-2b84-4e0d-9e6d-da0fd25d14bc)
- [MS-RDPEGFX specification, revision 19.0](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/)
