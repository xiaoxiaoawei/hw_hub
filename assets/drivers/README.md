# 内置 Windows USB 串口驱动

当前版本使用用户提供的两份驱动，运行时将完整驱动 ZIP 编入 FlashTool：

| 文件 | 用途 | SHA256 |
| --- | --- | --- |
| cp210x.zip | 上传的 CP210x 原始 ZIP，逐字节保留 | 3076ea2700fbadd9b50f01c7e5c8649c8254db9262114661e5f46ac183bd86ed |
| ch343.exe | 上传的 CH343SER.EXE 原文件，仅保留作溯源 | 99f16f9c4cf9c315dc9a17b29021d82d522014ecc053d9ee1c7b38c214dea40b |
| ch343.zip | 从上述 RAR 自解压 EXE 静态提取的完整驱动包 | b49881a847d9446d7e2e9b5ed1819f2db5468c673e3fe5ff4f494225e00cfae2 |

来源、完整文件清单、字节数和每个文件的哈希见 provenance.json。没有运行上传的 EXE，也没有修改 INF、CAT、SYS 或 DLL。ch343.zip 保留原 RAR 的全部 20 个文件及目录；原 EXE 同时保留在仓库。实际编入应用的 CH343 安装资源是 ch343.zip，应用不会启动其中的 SETUP.EXE 或 DRVSETUP64.exe。

## 静态检查与安装目标

CP210x 包的 DriverVer 为 10/04/2022,11.2.0.167，包含 silabser.inf、silabser.cat、x86/x64/ARM/ARM64 的 SYS、原始许可和发行说明。INF 的 NTamd64.10.0...17134 设备段支持 Windows 10 build 17134 及以上，对应 SYS 在 x64/silabser.sys。常见 CP2102 硬件 ID USB\VID_10C4&PID_EA60 包含在包内。

CH343 包的 DriverVer 为 05/19/2026,2.2.2026.05；NTamd64 设备段包含 CH343 的 USB\VID_1A86&PID_55D3 与 CH9102 的 PID_55D4，x64 安装段引用 CH343S64.SYS 及配套 DLL。

该 WCH 包有根目录和 WIN 1X 两份 CH343SER.INF。静态比较确认这两份 INF 和其余八份对应的 CAT/SYS/DLL 均逐字节一致。程序仅对白名单中的这两条路径、且九份配套文件全部相同时，确定选择 WIN 1X/CH343SER.INF 作为 Windows 10/11 安装候选。其他不明确的多 INF 布局会被拒绝；单 INF 的旧版官方 ZIP 仍可使用。INF SourceDisksFiles 中引用的各架构文件也必须完整存在。

ZIP 的路径、条目数量、展开大小及 CRC 已检查。原 EXE 的 PE 证书表在文件边界内，是 PKCS#7 SignedData 结构，含 Nanjing Qinheng Microelectronics Co., Ltd. 证书；两个 CAT 均能解析出 Microsoft Windows Hardware Compatibility Publisher 证书。这些是静态结构检查，未完成 Windows 证书信任、吊销或驱动安装验证，不能据此声称驱动已经受系统信任或安装成功。

## 许可原文

CP210x ZIP 内的 SLAB_License_Agreement_VCP_Windows.txt 原文额外保存为 cp210x-license.txt，字节不变。内置 CP210x 的安装入口先展示原文，用户接受后才启动安装。包内条款第 3、4 节涉及二进制修改、Silicon Labs 芯片范围、通知保留、再分发及终端用户协议要求；应阅读完整原文，不将本应用的开源依赖许可视为驱动的许可，也不声称获得了条款以外的再分发授权。

上传的 CH343 RAR 中未发现单独的许可文本；全部文件与原始 EXE 均完整保留，没有凭空补写厂商授权。

## 构建与手动安装

build.rs 根据 ZIP 的存在情况编入完整原包。0.4.0 及以后版本的 Windows 打包默认要求两种驱动均内置；历史版本仍可明确选择缺包构建。BUILD-METADATA.json 记录 embedded 状态以及实际 ZIP 文件名、大小和 SHA256；发布 ZIP 还保留本说明、provenance.json 和 CP210x 许可。

只有用户点击安装后，程序才解压到独立临时目录，验证芯片硬件 ID、目标 INF、CAT 结构和全部引用文件，然后通过 UAC 以管理员权限运行系统目录的 pnputil.exe /add-driver "<INF>" /install。程序等待真实退出码；0 表示 Windows 已成功处理驱动包，3010 表示还需重启，其他值显示失败。成功后仍需刷新串口列表或重新插拔以确认具体设备。取消 UAC 不会被报告为成功。

Windows PnPUtil 负责驱动包签名与系统信任的最终检查，程序不会禁用签名验证。Linux 上只进行静态解析与测试，不执行安装器。

也可在 EXE 旁的 drivers/ch343.zip、drivers/cp210x.zip 放置离线包，或手动选择 ZIP；界面区分相邻离线包和真正的内置资源。已有内置包会优先使用。scripts/fetch-drivers.ps1 保留为显式下载官方镜像的辅助工具，默认不会覆盖已提供的 ZIP；当前构建无需重新下载驱动。
