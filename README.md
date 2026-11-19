# FlashTool

轻量 Rust 原生桌面烧录工具，面向 Windows 10/11 x64 和 USB Hub 批量生产。无需安装 Python、Rust 或原乐鑫烧录工具。

[下载 Windows 版本](https://github.com/xiaoxiaoawei/hw_hub/releases) · [版本记录](CHANGELOG.md) · [驱动说明](assets/drivers/README.md) · [日志上传接口](docs/log-upload-api.md) · [软件更新说明](docs/software-updates.md)

![FlashTool 工作台](assets/ui-preview.png)

预览在 Linux 开发环境拍摄，因此显示手动刷新模式；Windows 发布包支持设备插拔自动识别。

## 工作流程

1. **选择芯片**：默认 ESP32-C3，其他型号可在芯片选择器中切换。工具会检查固件镜像头，并与实际连接的芯片比较。
2. **配置固件**：选择合并 BIN，或添加多个 BIN 和十六进制地址。常用分段模板会随芯片切换 bootloader 地址；实际地址以固件发布说明为准。
3. **勾选设备**：Windows 直接监听 USB 与串口接口的插拔事件，事件即触发首扫，忙时保留请求，再有限次补扫以等待驱动就绪，也可手动刷新；空闲时不持续轮询，新出现的端口不会自动勾选。拔出的设备从在线列表移除，旧勾选清除；本批烧录进度及结果另存为只读记录，不计入在线设备。
4. **开始批量烧录**：每台独立连接、检查 Flash 容量、压缩写入、MD5 校验并复位。仅全部步骤成功后标记成功；单台失败不影响其他设备。
5. **查看日志与保存结果**：每个 COM 口独立显示进度、状态和耗时；点击设备卡片的终端按钮，打开该设备的详细日志窗口并复制日志。底部“烧录汇总”显示设备结果和批次汇总；“清空汇总”保留设备日志及任务结果。每台任务结束后自动保存独立 CSV，也可手动导出本批汇总。

支持 1–16 台同时烧录，更多设备排队处理；默认并发 4 台，波特率 460800，可选择 115200 或 921600。“停止排队”仅取消尚未开始的设备，正在写入的设备会继续完成校验。

主界面使用紧凑暗色工作台，左下角齿轮打开“软件设置”，分为“通用设置”“驱动安装”“高级配置”三页，依次管理软件更新与本地 CSV、串口驱动、可选日志上报与身份提取规则。

## 设备记录与可选上报

每台设备的任务结束后，先把批次、COM、芯片身份、结果、时间、固件摘要和本台日志保存为独立 CSV，再处理可选的 HTTP 上报。Windows 默认目录为 `%LOCALAPPDATA%\FlashTool\records`，可在“软件设置 → 通用设置”选择或打开目录；保存失败会保留记录并提供重试入口。“导出本批 CSV”另外生成批次汇总，包含批次 ID、eFuse 出厂 MAC 和 MAC。

芯片身份直接来自本台设备的只读识别：eFuse ID 是出厂 MAC 的 12 位十六进制值，MAC 是同一值的冒号分隔形式。任务中的结构化身份与本台日志提取结果必须完整且一致；缺失或冲突时保留 CSV 和原因，跳过上报。

“软件设置 → 高级配置”中的日志上报默认关闭，接口地址为空。接入自己的服务端后，可配置 HTTP/HTTPS 地址、超时、身份提取规则和自定义请求头，再明确启用；没有预设的外部接收地址。网络失败不会改变烧录结果，已保存的 CSV 保持可用，可按当前设置手动重试上报。JSON 字段、幂等键、重试规则及联调示例见[日志上传接口](docs/log-upload-api.md)。

## 自动检查软件更新

默认在每天首次启动时按系统本地日期检查 GitHub 最新正式 Release。可在“软件设置 → 通用设置”中关闭自动检查，也可随时手动检查。检查完成后弹窗显示结果，失败时提供原因和重试入口。

从 **v0.5.2 起**，发现新版后显示发布说明，可选择“稍后”或“立即更新”。点击一次“立即更新”后，后台下载并核对大小、SHA256、Windows x64 程序及内嵌版本，再等待当前烧录、驱动安装、本地 CSV、未保存设置和已入队的 HTTP 上报处理完成，自动安装并重启。等待期间可“取消本次安装”，更新失败可“重试更新”。更新会保留设置和设备 CSV。

**v0.5.0 及更早版本首次升级仍需从官方 Release 手动下载、退出旧版并运行新版。** v0.5.1 升级时仍使用旧版“下载更新 → 重启更新”两次确认。一次确认流程从 v0.5.2 起生效。具体流程、可写便携目录要求及失败恢复见[软件更新说明](docs/software-updates.md)。

更新来源固定为本仓库 GitHub Releases。API 临时不可用时会尝试同仓库的官网最新正式版本及附件；两条途径均失败时显示具体原因。预发布和草稿不会作为升级提示；下载地址须属于本仓库的正式版本资产，按语义版本比较（例如 `0.10.0` 高于 `0.9.0`）。

更新失败时，错误说明与“复制错误详情”“打开官方发布页”始终位于更新窗口的可见区域；完整错误也会写入运行日志。复制内容包含程序版本及 API、官网检查的失败原因，便于排查间歇性错误。

## 芯片与地址

| 芯片 | 常用独立 bootloader 地址 | 合并镜像起点 |
| --- | --- | --- |
| ESP32、ESP32-S2 | `0x1000` | `0x0`，要求前 4 KiB 为 `0xFF` 填充，正确镜像头位于 `0x1000` |
| ESP32-S3、C2、C3、C6、H2 | `0x0` | `0x0` |

常用分区表地址为 `0x8000`、应用为 `0x10000`。固件地址始终以实际工程导出说明为准。ESP8266 不在本版本支持范围。

开始前会冻结固件内容，检查镜像芯片 ID、4 KiB 地址对齐、擦除范围重叠和 16 MiB 地址上限；连接后严格核验 Flash JEDEC ID 与实际容量。工具不执行整片擦除、不烧 eFuse、不启用加密或 Secure Boot。安全下载模式因无法提供本版本要求的校验而拒绝写入。

USB Hub 建议使用独立供电，先完成单台验证再增加并发。连接失败时可降至 115200，检查线材、供电、串口占用和下载电路；部分板卡需要按住 BOOT 后按 RESET。

### 只发现一个 COM 时

USB Hub 提供连接通道，串口数量取决于 Windows 实际识别到的独立设备。系统只枚举一个健康 COM 时，软件不能补出第二个物理串口。

1. 分别将两台设备直连电脑，确认每台都能在设备管理器“端口（COM 和 LPT）”中出现；检查对应驱动、数据线和供电。
2. 接回 Hub，确认两台同时连接时 Windows 仍显示两个不同的健康 COM。若只剩一个，尝试更换 Hub 插口、线材或独立供电。
3. 系统已有两个 COM、软件仍少一个时，手动刷新串口，记录设备管理器中的端口名称和 COM 号，并保留软件设备列表与错误日志，便于排查。已拔出的本批记录保留在“本批记录 · 已离线”，不计入在线设备数量。

## 手动安装驱动

“软件设置 → 驱动安装”提供 CH343/CH9102、CP2102/CP210x 两类手动安装入口（此前提到的 SP2102 按 CP2102 处理）。仅用户点击后才触发 UAC，由 Windows PnPUtil 校验并安装原厂驱动，程序等待实际退出码。

v0.4.0 内置上传的 CP210x Universal 驱动，以及从上传的 CH343 自解压包静态提取的完整签名驱动文件。软件运行时不需要联网下载驱动。Windows 安装时执行系统签名与信任检查；原包及许可保留于工程的驱动目录。详见[驱动说明](assets/drivers/README.md)。

## GitHub 构建成功邮件

邮件来自 **GitHub Actions 工作流完成通知**。软件中不配置邮箱，也不通过 Releases 订阅替代构建通知。

1. 在仓库确认 GitHub Actions 已启用，并关注（Watch）本仓库。
2. 打开 [GitHub 通知设置](https://github.com/settings/notifications)，在 **System → Actions** 选择 **Email**。
3. 取消 **Only notify for failed workflows**，保存，以包含成功构建。
4. 使用自己的 GitHub 账号推送代码或标签，或手动运行工作流进行验证。GitHub 根据触发者和个人通知偏好发送完成邮件，工作流 YAML 无法替账号开启此选项。

推送到 `Flash_tool` 或推送 `v*` 版本标签会触发 Windows 构建。版本标签或手动选择发布会创建正式 Release。构建产物生成后会写入成功摘要。官方说明见 [Actions 完成通知](https://docs.github.com/en/actions/concepts/workflows-and-actions/notifications-for-workflow-runs)。

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

每次软件更新先验证并提交到 `Flash_tool`，再推送该分支和对应版本标签。Windows 构建完成并正式发布后，核对 GitHub Release 的 Latest 标记和附件完整性。

```sh
python3 scripts/bump-version.py --help
python3 scripts/bump-version.py --check
# 先为下一版本写好 CHANGELOG，再预演并更新版本：
python3 scripts/bump-version.py 0.5.3 --dry-run
python3 scripts/bump-version.py 0.5.3
```

提交并验证新版本后创建对应 Git 标签，例如 `git tag -a v0.5.3 -m "FlashTool v0.5.3"`，再推送分支和标签以触发发布。标签必须与 Cargo 版本相同；发布后不移动标签、不覆盖已有资产，修复使用新版本。

GitHub Actions 验证 `vX.Y.Z` 标签与 Cargo 版本一致，通过格式、静态检查和测试后在 Windows 构建。正式 Release 统一使用 `FlashTool vX.Y.Z` 标题，上传版本 ZIP、源码 ZIP 和 SHA256 校验文件，验证成功后才发布并标记 Latest。已发布标签及附件保持不变；可手动选择只构建或发布一个已存在的版本标签。Latest 保持指向较高的正式版本，补发旧版本时保留更新版本的 Latest 标记。

从 v0.4.2 起，`FlashTool-vX.Y.Z-source.zip` 仅包含该标签中的 Rust 工程、测试、脚本、驱动资源及许可，排除仓库原有的旧软件安装包。已发布版本的附件及校验和保持不变；GitHub 自动生成的 “Source code” 归档仍由 GitHub 管理。

发布工具也可从本地验证过的标签和构建产物完成相同流程：

```sh
# 离线核对当前版本的标签、提交、Windows ZIP 与 SHA256SUMS
bash scripts/publish-release.sh --assets-dir dist --check
# 确认计划后访问 GitHub，上传并核验草稿附件，最后正式发布
bash scripts/publish-release.sh --assets-dir dist
```

发布工具要求已有的 Git 标签与 Cargo 版本一致，并且 Windows ZIP 及其校验文件已生成；网络或权限错误会中止发布。上传失败会保留未完成的草稿；GitHub 构建完成邮件与附件发布是独立的状态。

Windows 构建成功、仅 Release 发布失败时，可复用原构建重试：在 `.github/release-retry.json` 指定版本标签、原工作流运行 ID 和完整提交 SHA，推送到 `Flash_tool`，或手动运行 `Retry verified release`。流程会核对来源仓库、工作流、提交及 Windows 任务的成功状态，然后下载原产物、校验并续传草稿；不会移动标签或覆盖已发布附件。构建产物已过期时，使用原 Windows 工作流重新构建该标签。

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

自动测试覆盖多芯片固件预检、容量/擦除边界、并发上限、单台失败隔离、停止排队、逐 COM 日志和身份绑定、驱动包检查及 CSV 导出。上报测试使用 localhost 模拟服务，检查先落盘后请求、身份缺失/冲突跳过、超时、重试、重定向和请求头约束；实际业务服务端需另行接入。

云环境可验证 Linux 原生界面和 Windows 交叉构建，Windows 发布由 GitHub Actions 原生构建校验。尚未连接真实 Windows USB Hub 与 ESP 设备，插拔识别、多设备烧录及驱动安装仍需实机验收；新增芯片必须在相应实物上验证。

原始参考附件是乐鑫烧录工具发布包，并非源码。本工程没有分发其 EXE、私有固件或设备日志。
