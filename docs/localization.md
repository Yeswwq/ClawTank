# 界面本地化

ClawTank 使用 Apple String Catalog（`Localizable.xcstrings`）维护界面翻译。英文（`en`）是
开发语言和回退语言，简体中文使用 Apple 语言标识 `zh-Hans`。目录位于
`app/ClawTank/Resources/Localizable.xcstrings`，由 `app/project.yml` 中的 XcodeGen 目标
作为应用资源打包。新增文案仍以英文作为代码源字符串，目录保存对应的中文译文；这样上游更新代码时可以继续使用 Xcode 的提取、同步和过期条目标记流程。

## 语言切换

应用语言由 macOS 的“系统设置 → 通用 → 语言与地区 → 应用”管理。ClawTank 的“通用”设置页根据 `Bundle.main.preferredLocalizations` 显示当前实际采用的应用本地化，并提供打开“系统设置”的按钮；用户可在“应用”列表中选择 ClawTank。不要在代码中写入 `AppleLanguages` 或自行维护另一套语言引擎。首次运行遵循 macOS 当前语言偏好；系统首选语言为简体中文时，ClawTank 默认使用简体中文。修改后退出并重新打开 ClawTank，确保菜单栏、窗口标题、辅助功能标签和后台生成的本地化字符串都使用同一语言。

## 修改文案

- SwiftUI 的用户可见静态文案使用可提取的 `Text`、`Button`、`Toggle`、`Label`、`Section`、
  `TableColumn` 等 API。普通 Swift 代码、窗口标题、自定义控件参数和无障碍标签使用
  `String(localized:)`；不要把本地化文案存进不可提取的普通字符串后再传给界面。
- 一个本地化 key 表达完整句子。动态值通过插值传入，并保持其类型（例如整数数量），不要将
  英文句子拆成片段拼接。为译者补充必要的上下文，尤其是按钮含义、缩写和可互换的短词。
- 数量文案使用整数插值，并在 String Catalog 中维护语言对应的复数变体；不要写
  `item(s)` 这样的英语复数占位。中文译文按语义填写，不必仿照英语的单复数形式。
- 修改英文文案后，在 Xcode 中构建应用以提取并同步 String Catalog 条目；逐项检查新 key、
  占位符、译文状态以及被标为过期的旧 key。为新 key 补上 `zh-Hans`，移除确认已不再被源代码
  使用的条目。不要覆盖英文源 key 来放中文，也不要把中文硬编码进 Swift。
- 上游合并或 rebase 后，重新检查源文案与目录的 key 差异。若英文文案改变，确认新条目获得
  中文翻译，并将旧条目标为过期或删除；若仅有非语义格式变化，不要无条件复制出重复译文。
- 协议字段、命令行参数、环境变量、Agent API/JSON 字段、诊断格式和审计日志机器数据保持
  稳定，不随界面语言改变。日志文件保存固定的机器值；连接日志界面在展示层本地化状态。
- 系统设置名称和系统界面由 macOS 自行本地化；ClawTank 只翻译自己的说明与操作标题。

## 检查

在 macOS 和完整 Xcode 环境中运行：

```sh
xcodegen generate --spec app/project.yml
xcrun xcstringstool compile app/ClawTank/Resources/Localizable.xcstrings \
  --output-directory /tmp/clawtank-localization-check
make ARCH=x86_64 build
```

检查编译后的资源包含英文和 `zh-Hans`，并确认新 key 均有中文翻译、插值参数没有遗漏或误改。
在 Xcode Scheme 的 Run 选项中将 App Language 设为“简体中文”，检查菜单栏、设置、首次设置、
连接指南、远程访问、Agent 和连接日志。验证动态文字长度、状态变化、VoiceOver 标签及窗口布局；
设备没有中文系统语言时，可用相同的 Scheme 语言选项进行验证。构建成功只证明资源可编译和
x86_64 目标可构建，不代表 macOS 26 运行行为或 NVIDIA 硬件编码已验证。
