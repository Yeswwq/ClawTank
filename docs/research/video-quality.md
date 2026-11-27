# RDP 画质、自适应编码与后续路线

本分析基于当前分支工作区和固定 IronRDP revision `475e5d534f0afa080e51b940caf3d7c1f9d6ec6c`，核对日期为 2026-11-25。目标是把用户偏好、客户端能力、链路反馈与 macOS 编码器能力分开处理，避免把“画质”误当成单一 codec 开关。

## 当前链路

视频主路径为 ScreenCaptureKit 输出 IOSurface-backed NV12 → VideoToolbox H.264 → Rust FFI → IronRDP EGFX AVC420。ScreenCaptureKit 以 60 fps 上限和静态帧过滤采集；`H264Encoder` 要求硬件编码、关闭 B 帧并设置平均码率与突发上限；Rust 对未确认帧和编码器背压做有界管理。客户端没有协商 AVC 时走 BGRA 传统位图回退。

设置页使用 0–100 的连续画质偏好滑块：0、50、100 分别精确保留旧低带宽、均衡、高画质档位的 0.35×、1.0×、1.6×码率预算；帧率仍由单独的 30/60 fps 设置决定。运行时每秒汇总实际提交帧、跳帧、EGFX 在途帧、帧提交到 ACK 的延迟和 ACK 确认字节数。延迟包含服务端排队、传输和客户端处理/解码，不是纯网络 RTT；确认字节除以采样间隔得到的是当前画面负载下的应用层 goodput，不是链路可用带宽。

一个统计错误已在本轮修正：健康恢复使用实际成功提交给 RDP 的帧数，而不是包括未提交帧的完成编码回调数。Rust 侧按 frame ID 保存有界 ACK 样本，重复/未知 ACK 不累计字节；resize 和新会话清除旧样本。低复杂度或静态内容带来的低 goodput 不单独触发降码率，也不被当作可用带宽上限；控制器依靠跳帧、队列深度和 ACK 延迟判定拥塞，并只在有已确认流量的健康区间缓慢探测。没有 ACK 时显示等待确认且不恢复。ACK 确认延迟和 payload goodput 的统计可辅助诊断，但不能等同网络 RTT 或物理链路容量。

## AVC420 与 AVC444

AVC420 是 H.264 的 YUV 4:2:0 表示；对文字边缘和细彩色线条，色度子采样会降低清晰度。AVC444/AVC444v2 通过两个 4:2:0 视图按 MS-RDPEGFX 规定重组 YUV444，改善色度细节，但仍使用有损 H.264 量化，因此不能称为无损。它也不等价于“质量高就固定用 444、低带宽就固定用 420”：选择必须服从该连接实际协商的 EGFX capability，并考虑编码成本和互操作行为。

当前 ClawTank 的活跃路径只提取 AVC420 可用状态，并且只调用 `submit_h264` / IronRDP AVC420 sender。固定 IronRDP revision 虽已提供 AVC444 与 AVC444v2 的发送 API，但 ClawTank 尚未调用；现有采集和 VideoToolbox 设置走 NV12 / H.264 4:2:0，也没有将 YUV444 源拆成规范要求的主、辅助视图并维护其编码参考链的路径。仅检查 `supports_avc444()` 或切换 codec ID 会产生无效线流。

现有设置首版已落在 AVC420 码率预算上，不控制 codec。后续 AVC444 应单独分阶段实现：

1. 从连接已确认的 capability 派生 per-session 编码能力；取得保留足够色度精度的源图像，按 MS-RDPEGFX 构造主、辅助 YUV420 视图，并复用同一 H.264 decoder reference stream 的规范状态。
2. 维护参考帧、IDR、resize、背压和断连重置；用独立解码实现及 Windows App 各平台验证线格式和视觉输出。只有端到端通过后，才让画质偏好在会话建立时偏向 AVC444。

不要在同一阶段中动态来回切换 AVC420/444。codec 变化涉及编码参考链及客户端解码器状态；当前服务端缺少受测的切换/重建状态机。首版应在会话启动时按协商能力和偏好选择稳定路径，运行中先调码率，再在持续拥塞时谨慎降帧率。

## 速率自适应设计

RDP/TCP 没有本项目可以直接读取的“可用带宽”数值。画面越静态、纹理越少，编码输出越小，即使链路容量很高，ACK goodput 也会很低。因此该指标只能表示当前内容实际交付的压缩 payload，不能直接把目标码率限制在其某个比例；否则切换到高运动画面时会错误地沿用静态画面的低估值。要发现额外余量，需要在队列稳定、延迟稳定、有 ACK 的活动期小步探测，并观察队列/确认延迟是否恶化。

推荐的反馈顺序：

- 用户偏好决定编码器目标码率预算上限，分辨率和独立的帧率设置限制合理范围；goodput 作为观测值和 ACK 存在性判断，不作为硬上限。
- 健康时缓慢增加码率探测可用余量；背压、跳帧、in-flight 堆积或 ACK 延迟相对本会话基线持续上升时快速乘性退让。
- goodput 与 ACK 延迟采用平滑统计；码率在持续拥塞信号出现时乘性退让，在至少连续三个有提交且收到 ACK 的健康采样后缓慢探测，并设置上下限，避免 Wi-Fi 短时抖动造成来回震荡。
- 先降低码率；只有持续拥塞且码率已接近下限时才降低帧率。空闲静态桌面不应被判定为低速，缺少 ACK 时不应虚构 goodput。
- 记录估算的 ACK goodput、发送/确认字节、队列深度、编码耗时、码率目标、实际码率和退让原因，供受控复测复现。

当前已通过窄 FFI stats 字段提供累计 ACK 字节、在途帧数和提交到确认延迟。ACK 样本表最多保存 256 帧；容量溢出、resize 或关闭后到达的迟到 ACK 不进入统计。采样按约一秒的控制间隔计算 goodput，并在控制器中做 EWMA。后续如需按网络条件分类初始 profile，应将 MCS connection type 作为静态先验单独记录；不能称为实时测得的链路速率。

## xrdp 可借鉴点

截至 2026-11-25 检查的 xrdp `devel` 源码，图形 encoder 按连接协商能力和配置在每会话建立；其 H.264 EGFX 输入路径使用 NV12，并把 MCS `connection_type` 传给 H.264 编码器。xrdp 的软件 H.264 配置按 connection type 提供 preset、目标/最大码率和帧率；RemoteFX Progressive 还按 connection type 选标准、低或超低质量量化表。`AUTODETECT` 在相应分支仍标为未实现。这些信息是静态连接类别先验与管理员配置，不是实时 ACK 测出的剩余带宽。适合 ClawTank 借鉴的是能力驱动的每会话编码器、明确的输入像素格式、有界排队和分离的编码/发送工作；本项目不应为了模仿 xrdp 的 profile 而把客户端宣告的连接类型伪装成测得速率。

xrdp 的 EGFX 队列参数有界：默认在途帧数为 2，可配置范围 1–16，单个压缩缓冲区默认 3 MiB，并有上下限。对 ClawTank 最值得移植的设计是 per-session codec/encoder 状态、显式背压、有限在途帧数、编码与发送线程解耦，以及按网络类别提供初始 profile 后再由运行时反馈细调；不要把客户端声明的连接类别冒充实时速率。

xrdp 的代码可借鉴其每会话 encoder 和 profile 配置，但不能将其当作 ClawTank 必须复制的实现方案。AVC444/AVC444v2 仍需要逐层完成 capability gating、源色度转换、两个 YUV420 子流的共同解码参考链与 wire format；不能把客户端某个 capability 位当成完整编码功能。

对 ClawTank 的建议：Rust 为每连接保存已确认的 EGFX capability 和 in-flight/ACK 统计；Swift 负责每连接 `VideoToolbox` 实例和采集输入。用户滑块作为“画质/带宽偏好”，先变成码率上限和探测积极度，不直接等于 4:2:0/4:4:4 开关。待 AVC444 完整实现后，在会话建立时依据客户端确认能力、源像素/硬件编码器支持和偏好选路径；若运行中要切 codec，必须先设计并验证 IDR、参考帧清空、surface 重置、重协商及失败回退状态机。NVIDIA 仍只通过系统 VideoToolbox API 运行时发现，不进入 Rust 协议核心。

## HEVC

Microsoft 2026-05-11 发布的 MS-RDPEGFX 19.0 公开版本定义了 H.264 AVC420、AVC444 和 AVC444v2 的 codec ID/能力协商；没有给普通 `WireToSurface1` 定义 HEVC codec ID。不能因 VideoToolbox 或 NullMoth 驱动能编码 HEVC，就自行选一个 ID 或向 Windows App 发 HEVC 数据。

Apple VideoToolbox 提供 HEVC 编码 API，但能否硬件加速必须通过当前系统/驱动的 encoder capability 查询和会话属性验证。该能力只证明本机编码可行，不证明 Windows App 会解码，更不构成 RDP 协议协商。

当前公开 MS-RDPEGFX `WireToSurface1` codec ID 列表定义 AVC420、AVC444 和 AVC444v2，没有通用 HEVC 项；本项目固定的 IronRDP EGFX `Codec1Type` 也尚无 HEVC codec ID。微软确实为 Azure Virtual Desktop 提供硬件 HEVC 图形编码，但它走 AVD 专用主机策略，并限定受支持的客户端平台/版本；当前文档列出的 Windows App 要求是 Windows 上 2.0.503.0 或更高，不能据此推导 macOS/Android Windows App 或普通直连 RDP 支持同一条线协议。故 HEVC 暂不进入通用 Windows App 通道。后续门槛是 Microsoft 发布可引用的通用 RDP 线协议/能力协商契约、目标客户端实际声明和解码支持、IronRDP 正式提供协议路径，以及本机 VideoToolbox 硬件编码、延迟/功耗和真实客户端互操作均有结果。

## 优化顺序与验收

1. 保持已验证的 NV12→VideoToolbox→AVC420 热路径，修复真实提交/确认统计并完善结构化日志。
2. 加入偏好滑块和安全的 AVC420 码率探测/退让；用确定性控制器单测和受控吞吐、延迟、抖动场景验证。
3. 评估 AVC444 的采样转换和双视图编码成本；测量 CPU、GPU、内存带宽、发送字节、P50/P95 编码延迟及彩色细字质量，再决定是否值得默认启用。
4. 只有通用 RDP 协议明确支持 HEVC 且客户端互操作成立后，才扩展 codec 枚举和 VideoToolbox 编码器；按会话协商，保留 AVC420 回退。

画质滑块的语义为 0“优先省带宽”、100“优先画质”，中间值为连续码率预算偏好。当前映射在 0/50/100 精确锚定旧档位，并以实际提交/ACK、队列和确认延迟约束运行时码率；帧率仍由独立设置控制。它不承诺无损，也不切换编码器。未来 AVC444 完整实现后，可在连接建立时依据客户端确认能力、源像素质量、编码器能力与偏好选择稳定 codec；不要根据短时吞吐在同一 H.264 参考链中频繁切换。

构建、x86_64 XCTest、macOS 26 运行、Windows PC/Android Windows App 互操作、NullMoth/NVIDIA H.264/HEVC 硬件加速是不同的验收项，不能相互替代。

## 参考资料

- [Microsoft MS-RDPEGFX 协议版本与下载](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/)
- [Microsoft MS-RDPEGFX 版本和能力协商](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/31c6e2b1-335b-4a75-9454-bb2309958c21)
- [Microsoft YUV420p 视图重组为 YUV444](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/8131c1bc-1af8-4907-a05a-f72f4581160f)
- [Microsoft WireToSurface codec ID 定义](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/fb919fce-cc97-4d2b-8cf5-a737a00ef1a6)
- [xrdp encoder](https://github.com/neutrinolabs/xrdp/blob/devel/xrdp/xrdp_encoder.c)
- [xrdp connection/session manager](https://github.com/neutrinolabs/xrdp/blob/devel/xrdp/xrdp_mm.c)
- [xrdp H.264 per-connection-type configuration](https://github.com/neutrinolabs/xrdp/blob/devel/xrdp/xrdp_tconfig.c)
- [xrdp AVC444 in-process support gap and implementation discussion](https://github.com/neutrinolabs/xrdp/issues/3944)
- [xrdp VA-API H.264 encoder proposal](https://github.com/neutrinolabs/xrdp/issues/3886)
- [Microsoft MS-RDPEGFX current version and capability negotiation](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/31c6e2b1-335b-4a75-9454-bb2309958c21)
- [Apple VideoToolbox](https://developer.apple.com/documentation/videotoolbox)
- [Apple 低延迟视频编码指引](https://developer.apple.com/documentation/videotoolbox/encoding-video-for-low-latency-conferencing)
