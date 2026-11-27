# Contributing to ClawTank

Thanks for your interest! Bug reports, test reports, documentation fixes and code are all welcome. By taking part you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

- **Found a bug?** Open a [bug report](https://github.com/HMAKT99/ClawTank/issues/new?template=bug_report.yml) and attach **Export Diagnostics…** from the menu.
- **Testing the beta?** Follow [docs/beta-testing.md](docs/beta-testing.md) and file a [beta test report](https://github.com/HMAKT99/ClawTank/issues/new?template=beta_report.yml).
- **Security issue?** Don't open a public issue. See [SECURITY.md](SECURITY.md).
- **Bigger change?** Open an issue first, so we can agree on the approach before you invest time.

## 互操作分析与资料复用边界

ClawTank 的实现依据包括微软公开的 RDP 规范（[MS-RDPBCGR]、[MS-RDPEGFX] 等）、
Apple 公开文档、已在 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) 中注明且
许可允许复用的开源代码，以及项目所有者提供或授权用于互操作分析的客户端工件。

允许为互操作目的检查闭源远程桌面客户端工件，包括协议行为、能力协商、媒体和
输入处理。记录工件观察结果，并与公开规范要求及线上验证结果区分。不得复制或
分发专有源代码、资源、界面文案或品牌内容；兼容实现应独立编写，并尽可能通过
公开规范和跨实现测试验证。

## Development setup

要求：macOS 14+ 的 Mac、完整 Xcode 16+、通过官方 [rustup](https://rustup.rs)
安装并管理的 Rust stable、XcodeGen 和 SwiftLint。Apple Silicon 是上游发布基线；
Intel/x86_64 支持在 [`docs/macos26-intel-nvidia-rdp.md`](docs/macos26-intel-nvidia-rdp.md)
中单独跟踪。使用 rustup 安装 Rust，并在当前 shell 启用其工具链：

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

在使用 MacPorts 的主机上，可执行以下命令安装 XcodeGen 和 SwiftLint：

```sh
sudo port install xcodegen swiftlint
```

MacPorts 安装这两个工具前需要完整 Xcode；只有 Command Line Tools 时会被
拒绝。安装完整 Xcode 后，再运行上述命令并确认 `xcodegen`、`swiftlint` 在
`PATH` 中。

```sh
git clone https://github.com/HMAKT99/ClawTank.git && cd ClawTank
make dev-signing   # once: a local code-signing identity, so macOS permissions survive rebuilds
make build         # Rust core + app → app/build/Build/Products/Release/ClawTank.app
make test          # cargo test + XCTest
make lint          # clippy -D warnings, rustfmt --check, swiftlint --strict
```

For Xcode, run `make core project` and open `app/ClawTank.xcodeproj`. The project is generated from `app/project.yml` by XcodeGen, so edit that file, not the `.xcodeproj`.

## 分支与上游同步

保持 `main` 与 `upstream/main` 对齐，并在聚焦分支上开发功能。Intel/NVIDIA
工作以 [`docs/macos26-intel-nvidia-rdp.md`](docs/macos26-intel-nvidia-rdp.md)
作为范围和验收约定。发布分支前，先 rebase 到最新上游基线，再运行相关 lint
和测试命令：

```sh
git fetch upstream
git rebase upstream/main
make lint test
```

不要把生成的 Xcode 工程、构建产物、签名密钥或无关重构混入协议或 macOS
集成改动。

| Target | What it does |
|---|---|
| `make test-core` / `make test-app` | Only the Rust or only the Swift tests |
| `make bench` | Encoder benchmark (2560×1600 @ 60 fps) |
| `make audit` | `cargo audit` of Rust dependencies |
| `make package` / `make release` | Unsigned DMG / versioned release artifacts (see [docs/releasing.md](docs/releasing.md)) |

**README screenshots** are rendered from the real views with sample data: `TEST_RUNNER_CLAWTANK_SCREENSHOTS=$PWD/docs/media make test-app`.

**Testing against the running app without a GUI client.** `core/clawtank-core/examples/probe.rs` connects like a real client (TLS and NLA), and can measure video, input, resize and audio. The usage is in [docs/testing.md](docs/testing.md). Some live tests take over the mouse and keyboard, and the real-display tests are opt-in (`TEST_RUNNER_CLAWTANK_VD_TESTS=1`).

## Where things go

Read [docs/architecture.md](docs/architecture.md) first. The key rule: **Rust never calls macOS APIs, and Swift never parses RDP.**

- **Protocol work** goes in `core/clawtank-core/src/`, with an end-to-end test in `core/clawtank-core/tests/`. Those tests use IronRDP's client.
- **macOS work** goes in `app/ClawTank/<Area>/`, with XCTest in `app/ClawTankTests/`. Inject fakes for system APIs.
- **New C ABI functions** go only in `core/clawtank-core/src/ffi.rs`, with a Swift wrapper in `CoreBridge.swift` and tests. Keep the boundary narrow (ADR-002).
- **Significant or hard-to-reverse choices** get an ADR in [docs/decisions.md](docs/decisions.md).
- **界面本地化**遵循 [docs/localization.md](docs/localization.md)：使用英文源字符串和
  Apple String Catalog；为用户可见文案同步维护 `zh-Hans`，不要本地化协议字段或机器数据。

## Conventions

- Every source file starts with a comment explaining what it does and why.
- Swift 6 strict concurrency, with no warnings. Rust with `clippy -D warnings`.
- No new dependencies without a reason in the PR. Licenses must be compatible with Apache-2.0, and they go in THIRD_PARTY_NOTICES.md.
- Never log secrets. Audit-log entries describe actions, not content (for example, the character count of typed text, not the text).
- Commits: an imperative subject line, prefixed by area when that helps (`core:`, `app:`, `docs:`). Keep changes focused.
- User-visible changes go in [CHANGELOG.md](CHANGELOG.md) under *Unreleased*.

## Pull requests

1. Fork, create a branch, and make your change with tests.
2. `make lint test` passes locally. CI runs it on macOS, plus a FreeRDP interop test and `cargo audit`.
3. Fill in the PR template, including how you tested it (client, macOS version).

By contributing, you agree that your work is licensed under [Apache-2.0](LICENSE).

[MS-RDPBCGR]: https://learn.microsoft.com/openspecs/windows_protocols/ms-rdpbcgr/
[MS-RDPEGFX]: https://learn.microsoft.com/openspecs/windows_protocols/ms-rdpegfx/
