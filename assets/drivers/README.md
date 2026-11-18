# Windows USB 串口驱动包

本目录支持两个可选的原厂完整 ZIP：

| 文件 | 芯片 | 预期 INF / 常见硬件 ID |
| --- | --- | --- |
| `ch343.zip` | WCH CH343 / CH9102 | `CH343SER.INF`，`USB\VID_1A86&PID_55D3` / `55D4` |
| `cp210x.zip` | Silicon Labs CP2102 / CP210x | `silabser.inf`，`USB\VID_10C4&PID_EA60` |

仓库目前没有附带驱动二进制。缺包不会导致编译失败，界面会准确显示“未打包”，仍可由用户选择官方 ZIP 安装。不能将占位文件或其他芯片的驱动改名后用作驱动包。

## 获取及打包

在安装了 PowerShell 的 Windows 机器上，从仓库根目录运行：

```powershell
powershell -File scripts/fetch-drivers.ps1
```

脚本仅下载和检查，不安装驱动。它使用 Espressif 官方 Windows 安装器采用的下载镜像：

- CH343：<https://dl.espressif.com/dl/idf-installer/CH341SER.ZIP>
- CP210x：<https://dl.espressif.com/dl/idf-installer/CP210x_Universal_Windows_Driver.zip>

原厂页面：

- WCH：<https://www.wch.cn/downloads/CH343SER_ZIP.html>
- Silicon Labs：<https://www.silabs.com/developers/usb-to-uart-bridge-vcp-drivers>

镜像及 INF 路径的参考是 Espressif [`idf-env/src/driver.rs`](https://github.com/espressif/idf-env/blob/cc4b7c3fb309b93fc3828b29553bddcd19fe41bd/src/driver.rs)。CH343 镜像文件名虽为 `CH341SER.ZIP`，实际包内仍必须包含 CH343 对应的 INF、硬件 ID、CAT 及 SYS；脚本和程序会检查，不能仅按下载文件名判断兼容性。

每次下载记录 SHA256、字节数、来源、时间及包内许可/说明文件名，分别写入 `*.sha256` 和 `*.source.json`。这是下载记录，不是厂商公布的校验值。已审阅的哈希可用于固定后续构建输入：

```powershell
powershell -File scripts/fetch-drivers.ps1 -Driver cp210x -ExpectedSha256 <已审阅的64位SHA256>
```

脚本默认保留已有 ZIP，只有显式传入 `-Force` 才替换；TLS 和证书验证始终开启。当前云环境到上述厂商及 Espressif 下载域名的请求被代理以 CONNECT 403 拒绝，因此这里尚未获取到原厂包，也没有核验其具体版本、签名或重分发条款。

重新编译时，`build.rs` 将存在的 ZIP 原样编入 EXE。完整 ZIP 保留原厂 CAT、SYS、许可和说明文件。发布包含驱动包的 EXE 前，需实际阅读随包及厂商页面的再分发条款；当前未确认两家具体授权条件，不能把其他开源项目的许可当作这些驱动二进制的授权。

若不重新编译，也可将 ZIP 放到发布 EXE 旁的 `drivers/ch343.zip`、`drivers/cp210x.zip`。界面会将这种来源标为相邻离线包，区分于真正的编译内置包；选择 ZIP 直接安装也受支持。

## 手动安装行为

只有用户点击安装按钮后才解压到独立临时目录，并检查路径、条目数、展开大小、目标 INF、USB 硬件 ID、SYS 及 INF 引用的 CAT 结构。程序保留完整包，调用 Windows 系统目录中 `pnputil.exe /add-driver "<INF>" /install`，通过 UAC 请求管理员权限，等待实际退出码：`0` 表示成功，`3010` 表示成功但需要重启，其他值显示失败；取消授权不会报告安装成功。

CAT 预检查只检查文件结构；Windows 的驱动包签名和证书信任校验才是安装依据。程序不会禁用签名校验、自动安装驱动、修改设备为不相关的 USB 驱动，或执行下载包里的 EXE。驱动包进入驱动存储后，设备是否匹配和是否需要重启仍由 Windows 决定。安装完成后可刷新串口列表或重新插拔设备。
