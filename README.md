# FlashTool

轻量 Rust 原生桌面烧录工具，面向 Windows 10/11 x64 和 USB Hub 批量生产。无需安装 Python、Rust 或原乐鑫烧录工具。

[下载 Windows 版本](https://github.com/xiaoxiaoawei/hw_hub/releases) · [版本记录](CHANGELOG.md) · [驱动说明](assets/drivers/README.md)

![FlashTool 工作台](assets/ui-preview.png)

预览在 Linux 开发环境拍摄，因此显示手动刷新模式；Windows 发布包支持设备插拔自动识别。

## 工作流程

1. **选择芯片**：默认 ESP32-C3，其他型号可在芯片选择器中切换。工具会检查固件镜像头，并与实际连接的芯片比较。
2. **配置固件**：选择合并 BIN，或添加多个 BIN 和十六进制地址。常用分段模板会随芯片切换 bootloader 地址；实际地址以固件发布说明为准。
3. **勾选设备**：Windows 直接监听 USB 与串口接口的插拔事件，设备变化后自动识别，也可手动刷新；空闲时不再定时扫描，新出现的端口不会自动勾选。仅在插拔事件后执行有限次数的延迟识别，以等待驱动就绪。
4. **开始批量烧录**：每台独立连接、检查 Flash 容量、压缩写入、MD5 校验并复位。仅全部步骤成功后标记成功；单台失败不影响其他设备。
5. **查看进度与保存结果**：每个 COM 口独立显示进度条、百分比、状态和耗时；导出含芯片、固件布局摘要、耗时及结果的 CSV。日志支持随时清空，设备进度和结果保持可用。

支持 1–16 台同时烧录，更多设备排队处理；默认并发 4 台，波特率 460800，可选择 115200 或 921600。“停止排队”仅取消尚未开始的设备，正在写入的设备会继续完成校验。

## 自动检查软件更新

启动后在后台检查 GitHub 最新正式 Release，每天最多自动请求一次；侧栏“检查更新”可随时手动查询，也可在更新窗口关闭自动检查。检查失败会显示网络错误并保留重试入口；没有正式 Release 时明确提示尚未发布。

发现比当前版本更新的正式版时显示版本号、更新说明和官方 Windows 下载入口；若附件尚未上传，显示“发布包尚未上传”并提供发布页。下载由用户选择，烧录任务保持独立运行。设置保存于 Windows 的 `%LOCALAPPDATA%\FlashTool` 用户目录。

更新来源固定为本仓库 GitHub Releases。预发布和草稿不会作为升级提示；下载地址须属于本仓库的正式版本资产，按语义版本比较（例如 `0.10.0` 高于 `0.9.0`）。

## 芯片与地址

| 芯片 | 常用独立 bootloader 地址 | 合并镜像起点 |
| --- | --- | --- |
| ESP32、ESP32-S2 | `0x1000` | `0x0`，要求前 4 KiB 为 `0xFF` 填充，正确镜像头位于 `0x1000` |
| ESP32-S3、C2、C3、C6、H2 | `0x0` | `0x0` |

常用分区表地址为 `0x8000`、应用为 `0x10000`。固件地址始终以实际工程导出说明为准。ESP8266 不在本版本支持范围。

开始前会冻结固件内容，检查镜像芯片 ID、4 KiB 地址对齐、擦除范围重叠和 16 MiB 地址上限；连接后严格核验 Flash JEDEC ID 与实际容量。工具不执行整片擦除、不烧 eFuse、不启用加密或 Secure Boot。安全下载模式因无法提供本版本要求的校验而拒绝写入。

USB Hub 建议使用独立供电，先完成单台验证再增加并发。连接失败时可降至 115200，检查线材、供电、串口占用和下载电路；部分板卡需要按住 BOOT 后按 RESET。

## 手动安装驱动

提供 CH343/CH9102、CP2102/CP210x 两类手动安装入口（此前提到的 SP2102 按 CP2102 处理）。仅用户点击后才触发 UAC，由 Windows PnPUtil 校验并安装原厂驱动，程序等待实际退出码。

v0.4.0 内置上传的 CP210x Universal 驱动，以及从上传的 CH343 自解压包静态提取的完整签名驱动文件。软件运行时不需要联网下载驱动。Windows 安装时执行系统签名与信任检查；原包及许可保留于工程的驱动目录。详见[驱动说明](assets/drivers/README.md)。

## GitHub 构建成功邮件

邮件来自 **GitHub Actions 工作流完成通知**。软件中不配置邮箱，也不通过 Releases 订阅替代构建通知。

1. 在仓库确认 GitHub Actions 已启用，并关注（Watch）本仓库。
2. 打开 [GitHub 通知设置](https://github.com/settings/notifications)，在 **System → Actions** 选择 **Email**。
3. 取消 **Only notify for failed workflows**，保存，以包含成功构建。
4. 使用自己的 GitHub 账号推送代码或标签，或手动运行工作流进行验证。GitHub 根据触发者和个人通知偏好发送完成邮件，工作流 YAML 无法替账号开启此选项。

本项目对 `main`、`flashtool` 分支推送以及 `v*` 标签执行 Windows 构建；只有版本标签或明确选择发布时创建正式 Release。构建产物生成后会写入成功摘要。官方说明见 [Actions 完成通知](https://docs.github.com/en/actions/concepts/workflows-and-actions/notifications-for-workflow-runs)。

## 构建与验证

安装 `rust-toolchain.toml` 指定的 Rust。Windows 原生构建还需要 MSVC Build Tools 的 Desktop development with C++。

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo run --locked
```

```powershell
# v0.4.0 起发布构建要求两种已提供的驱动均内置
.\scripts\build-windows.ps1 -RequireDrivers
# 显式下载辅助入口仅用于缺少资源时；不会覆盖已有的上传包
.\scripts\build-windows.ps1 -FetchDrivers
```

版本由 `Cargo.toml` 的 package.version 管理。界面、`--version`、Windows 文件版本资源和发布包名从该值生成；`Cargo.lock` 与标签必须一致。变更版本时使用版本工具，并在 CHANGELOG 中记录面向用户的变化。已发布标签保持不变，后续修复使用新版本。

```sh
python3 scripts/bump-version.py --help
python3 scripts/bump-version.py --check
# 先为下一版本写好 CHANGELOG，再预演并更新版本：
python3 scripts/bump-version.py 0.5.0 --dry-run
python3 scripts/bump-version.py 0.5.0
```

提交并验证新版本后创建对应 Git 标签，例如 `git tag -a v0.5.0 -m "FlashTool v0.5.0"`，再推送分支和标签以触发发布。标签必须与 Cargo 版本相同；发布后不移动标签、不覆盖已有资产，修复使用新版本。

GitHub Actions 验证 `vX.Y.Z` 标签与 Cargo 版本一致，通过格式、静态检查和测试后在 Windows 构建。正式 Release 统一使用 `FlashTool vX.Y.Z` 标题，上传版本 ZIP、源码 ZIP 和 SHA256 校验文件，验证成功后才发布并标记 Latest。已发布标签及附件保持不变；可手动选择只构建或发布一个已存在的版本标签。Latest 保持指向较高的正式版本，补发旧版本时保留更新版本的 Latest 标记。

发布工具也可从本地验证过的标签和构建产物完成相同流程：

```sh
# 离线核对当前版本的标签、提交、Windows ZIP 与 SHA256SUMS
bash scripts/publish-release.sh --assets-dir dist --check
# 确认计划后访问 GitHub，上传并核验草稿附件，最后正式发布
bash scripts/publish-release.sh --assets-dir dist
```

发布工具要求已有的 Git 标签与 Cargo 版本一致，并且 Windows ZIP 及其校验文件已生成；网络或权限错误会中止发布。上传失败会保留未完成的草稿；GitHub 构建完成邮件与附件发布是独立的状态。

Windows 构建成功、仅 Release 发布失败时，可复用原构建重试：在 `.github/release-retry.json` 指定版本标签、原工作流运行 ID 和完整提交 SHA，推送到 `flashtool`，或手动运行 `Retry verified release`。流程会核对来源仓库、工作流、提交及 Windows 任务的成功状态，然后下载原产物、校验并续传草稿；不会移动标签或覆盖已发布附件。构建产物已过期时，使用原 Windows 工作流重新构建该标签。

### 只读命令行

```sh
cargo run --locked -- --version
cargo run --locked -- --list-ports
cargo run --locked -- --check '0x0:/path/merged.bin'
cargo run --locked -- --check --chip esp32s3 '0x0:/path/merged-s3.bin'
```

`--check` 只检查文件，不连接设备、不写入 Flash。摘要包含所选芯片、分段地址、长度和内容。

### 云环境

使用现有 `/workspace/hw_hub` checkout。每个云任务已隔离，不需要额外创建 Git worktree。

```sh
export CARGO_HOME=/workspace/.toolchains/cargo
export RUSTUP_HOME=/workspace/.toolchains/rustup
export PATH="$CARGO_HOME/bin:$PATH"
bash scripts/setup-cloud.sh
# 本次准备的 MinGW 支持 Linux → Windows 交叉构建
source /workspace/.toolchains/mingw/env.sh
cargo build --locked --release --target x86_64-pc-windows-gnu -j 4
```

## 验证边界

自动测试覆盖多芯片固件预检、容量/擦除边界、并发上限、单台失败隔离、停止排队、进度、驱动包检查和 CSV 导出。云环境可验证原生界面与 Windows 交叉构建，未连接真实 ESP 设备，不能代替 Windows 实机量产验收；新增芯片必须在相应实物上验证。

原始参考附件是乐鑫烧录工具发布包，并非源码。本工程没有分发其 EXE、私有固件或设备日志。
