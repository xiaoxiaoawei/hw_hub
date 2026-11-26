# ESP32-S3 蓝牙验证工装 v0.1.1

将本固件烧录到独立 ESP32-S3 工装板。短按 GPIO7 后，工装主动扫描名称含 Lumysi 的待测设备，并通过串口报告实际观察到的广播数据。FlashTool v0.6.1 可直接监听并复核同一份数据。

## 接线与运行

- 板载 USB 转 TTL 使用 UART0，TX 为 GPIO43，RX 为 GPIO44。串口参数为 **115200 8N1**，流控关闭。
- GPIO7 为低有效按键，使用内部上拉。短按约 40–1000 毫秒，松开后开始扫描。
- GPIO1 输出 4 kHz、50% 占空比的蜂鸣器信号。扫描开始一声，通过两声，失败三声。
- 支持原生 USB Serial/JTAG 作为备用日志通道。带 USB 转 TTL 的开发板使用桥接芯片对应的 COM 即可。

固件先初始化 UART0 并输出 `BOOT`。随后持续输出状态，让晚打开的串口工具也能看到就绪或初始化失败。就绪状态下直接短按 GPIO7。每次短按对应一轮验证，FlashTool 可以保持连接并连续读取结果。

## 广播判定

每轮最多扫描 45 秒。主广播与扫描响应按实际 MAC 及地址类型归到同一候选。首次收齐通过条件后继续观察约 2 秒。该轮观察到多个 Lumysi 候选时返回失败，现场验证时一次放入一台待测板。

通过条件包括以下三项。

1. 主广播的 128 位服务 UUID 列表包含 `4fafc201-1fb5-459e-8fcc-c5c9c331914b`。
2. 同一设备实际扫描响应中的完整名称字段 `0x09` 含 `Lumysi`，匹配时忽略 ASCII 字母大小写。
3. 同一条扫描响应包含 `0xFF` 厂商数据。公司标识为 `FF FF`，诊断标记为 `4C`，四字节大端故障位全为零，固件版本为 `1.188`。末尾两字节大端电池采样值供记录使用。

带零故障位的诊断字段结构如下，`xx xx` 为实际电池采样值。

```text
0D FF FF FF 4C 00 00 00 00 01 00 BC xx xx
```

## 串口日志协议 v2

普通启动和运行日志为文本行。提取验证记录时读取以 `BLE_VERIFY ` 开头的 UTF-8 JSON 行。

- `ready` 提供 `session` 和 `protocol: 2`。`session` 是每次启动生成的 64 个小写十六进制字符。
- `start` 提供同一 `session` 与本次编号 `attempt`。
- `result` 提供当前 `session`、`attempt` 和 `status`。通过状态为 `PASS！`，失败状态为 `FAIL`，原因由 `reason` 提供。可用字段包括实际 `mac`、`name`、`firmware`、`fault_bits`、`battery_mv`、`adv_hex` 与 `scan_rsp_hex`。

当前广播规格提供实际 BLE MAC、名称和诊断字段。CSV 中的 UID 保留为空，供未来有 UID 来源的协议版本使用。

## 构建与烧录

使用官方 ESP-IDF **v5.3.2** 的 ESP32-S3 工具和主机 `cc`。在 Linux 或 macOS 环境执行以下命令。

```sh
source /path/to/esp-idf/export.sh
bash firmware/ble-verifier/scripts/build.sh /tmp/ble-verifier-build /tmp/ble-verifier-export
```

构建脚本在仓库外生成和校验引导程序、分区表、应用及合并镜像，版本取自 ESP-IDF 的构建元数据。镜像参数为 ESP32-S3、4 MB Flash、DIO 40 MHz。FlashTool 中选择 **ESP32-S3**，读取 `images/ESP32S3-BLE-Verifier-v0.1.1-merged.bin`，烧录地址填 **`0x0`**。分段地址由固件包内的 `flash-manifest.json` 给出。
