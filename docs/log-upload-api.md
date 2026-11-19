# 设备日志上传接口

本文定义 FlashTool 日志客户端的 JSON 接口，供后续业务服务端对接。上传默认关闭，endpoint 默认为空；未启用时仍保存本地设备记录。启用前应填写自己的服务地址，没有预设的公共接收服务器。

## 记录与设备身份

每条记录对应一个 batch_id 内的一台设备，port、身份、烧录结果和日志来自同一次任务。record_id 是客户端根据 batch_id 和 port 生成的稳定标识，格式为 record- 加 64 位 SHA256 十六进制值。服务端将它作为不透明标识使用。自动重试继续使用同一记录和同一标识；服务端应按 record_id 或 Idempotency-Key 幂等落库。

efuse_id 是芯片出厂 factory eFuse MAC 的 48 位原始值，以 12 位大写十六进制表示；mac 是同一值的冒号分隔表示。例如：

~~~text
EFUSE_ID=246F28112233 MAC=24:6F:28:11:22:33
~~~

这两个字段表示同一个 factory_mac，不是两组独立的 eFuse bank。记录不包含安全密钥、Flash 加密密钥或其他任意 eFuse 内容。

上传前同时检查后端提供的 DeviceIdentity 和本次设备日志的正则提取结果。两类标识必须完整、唯一、一致，且 efuse_id 去格式后的值必须与 mac 相同。全 0、全 FF 也视为无效身份。身份缺失、出现冲突、日志与后端身份不匹配时，只保存本地 CSV，跳过上传并保留原因；不会拿其他 COM 的值补齐。

## 请求与响应

~~~http
POST /api/flash-records HTTP/1.1
Content-Type: application/json
Idempotency-Key: <record_id>
~~~

路径由 endpoint 决定。支持 HTTP 和 HTTPS；HTTPS 使用正常证书校验。客户端不跟随任何重定向，包括同站重定向，因此 endpoint 应直接指向最终接收地址。URL 不允许内嵌用户名、密码或 fragment。

正文使用 UTF-8 JSON，键名为 snake_case，版本键是 schema_version。

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| schema_version | integer | 当前固定为 1。 |
| record_id | string | 稳定记录标识，同时作为 Idempotency-Key。 |
| batch_id | string | 批次标识；同批设备共享此值。 |
| port | string | 本次任务的端口，例如 COM7。 |
| chip | string | 芯片标识，例如 esp32c3。 |
| efuse_field | string | 固定为 factory_mac，说明 efuse_id 的实际来源。 |
| efuse_id | string 或 null | 12 位十六进制出厂 MAC；身份异常记录可为空。 |
| mac | string 或 null | 同一出厂 MAC 的冒号分隔格式。 |
| status | string | success、failed 或 cancelled，表示烧录任务结果。 |
| started_at | string | RFC 3339 时间。 |
| finished_at | string | RFC 3339 时间。 |
| duration_ms | integer | 任务耗时，单位毫秒。 |
| firmware_sha256 | string | 本批次已冻结的固件内容及地址布局摘要。 |
| logs | array of string | 该设备本次任务的日志行快照。 |
| identity_error | string 或 null | 身份校验异常原因；实际上传要求身份校验通过。 |
| validation_error | string 或 null | 时间或固件 SHA256 格式异常原因；异常记录仅存 CSV，不上传。 |

请求正文示例；示例中的固件摘要用于说明格式：

~~~json
{
  "schema_version": 1,
  "record_id": "record-8880d857e744ac27d8ae46954d2866f58620d6082fb8c6f5222eeae783029b7c",
  "batch_id": "batch-example-20261119-001",
  "port": "COM7",
  "chip": "esp32c3",
  "efuse_id": "246F28112233",
  "efuse_field": "factory_mac",
  "mac": "24:6F:28:11:22:33",
  "status": "success",
  "started_at": "2026-11-19T03:25:00Z",
  "finished_at": "2026-11-19T03:25:03Z",
  "duration_ms": 3000,
  "firmware_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "logs": [
    "EFUSE_ID=246F28112233 MAC=24:6F:28:11:22:33",
    "Flash verified"
  ],
  "identity_error": null,
  "validation_error": null
}
~~~

烧录失败与上传失败是不同结果：身份有效的 failed 任务也可以上报。HTTP 2xx 表示服务端已接收记录，不会把烧录结果改为 success。

服务端可返回 204 空响应，或不超过 64 KiB 的 2xx 响应正文。客户端只检查 HTTP 状态码，不依赖某个响应 JSON 字段。请求 JSON 上限为 1 MiB。

| 情况 | 客户端行为 |
| --- | --- |
| HTTP 200–299 | 上传成功。 |
| HTTP 300–399 | 失败，不跟随 Location，不自动重试。 |
| HTTP 408、425、429、500、502、503、504，或可重试的连接中断/超时 | 最多尝试 3 次，重试前分别等待 100、200 毫秒。 |
| 其他 HTTP 错误、证书验证失败、无效配置或超限响应 | 失败，不继续自动重试。 |

3 次是含首次请求在内的总次数；每次请求使用相同正文及幂等键。默认单次超时 10 秒，可配置为 5–30 秒。服务端应在持久化记录后再返回 2xx，并对相同幂等键的重复请求返回成功，避免客户端超时后重试造成重复记录。

## 客户端设置

TelemetrySettings 的默认配置：

~~~json
{
  "upload_enabled": false,
  "endpoint": "",
  "timeout_seconds": 10,
  "efuse_pattern": "EFUSE_ID=([0-9A-Fa-f]{12})",
  "mac_pattern": "MAC=([0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5})",
  "custom_headers": {},
  "csv_directory": null
}
~~~

在“软件设置”窗口的“本地记录”“日志上报”及“高级设置 · 身份提取与请求头”中编辑，点击“保存设置”后生效。批次使用启动时的设置快照，显式重试按下面的规则选择设置。

正则的第 1 个捕获组用于提取身份，不能代替后端身份校验。规则长度为 1–4096 字节，使用 Rust regex 支持的语法。重复出现同一个规范化值可以通过，出现多个不同值则拒绝绑定。调整正则后仍执行完整性、唯一性和两种 MAC 格式的一致性检查。

custom_headers 可用于服务端要求的 Authorization、X-API-Key 等请求头。客户端始终设置 Content-Type 和 Idempotency-Key，用户不能覆盖这些字段。请求头不写入上传 JSON 或设备 CSV。

最多可配置 32 个请求头；名称最长 128 字节，值最长 8192 字节，总计不超过 16 KiB。名称不区分大小写，值只接受可移植的 ASCII 文本及制表符；禁止换行和控制字符。Host、Content-Length、Connection、Transfer-Encoding、User-Agent、代理相关头等传输字段由客户端管理，配置校验会拒绝覆盖。

配置文件位于 Windows 的 %LOCALAPPDATA%\FlashTool\telemetry.json；Linux 使用 $XDG_CONFIG_HOME/FlashTool/telemetry.json，未设置时为 ~/.config/FlashTool/telemetry.json。自定义请求头保存在本地设置中，设置文件不是上传记录的一部分。

## 本地记录与失败处理

客户端先成功保存每条设备记录，再安排 HTTP 上传。每条记录独立保存为 record_id.csv，采用 UTF-8 BOM、CSV 引号转义和公式前缀防护，并在报告本地保存成功前同步到磁盘。HTTP 工作在独立队列中，单个上报超时不会阻塞后续设备记录的本地保存。

默认目录：

| 平台 | 默认设备记录目录 |
| --- | --- |
| Windows | %LOCALAPPDATA%\FlashTool\records |
| Linux | $XDG_DATA_HOME/FlashTool/records；未设置时使用 ~/.local/share/FlashTool/records |

csv_directory 可指定其他绝对路径。本地保存失败时不发送该记录，界面保留失败状态，供选择可写目录后重试。上传关闭、身份校验失败、HTTP 失败或上传队列暂满时，都应先查看该记录的本地保存结果，不能把上报状态当成本地文件状态。

每个自动保存文件包含表头和一行设备记录，列顺序如下。logs 单元格用换行连接日志行；CSV 中缺失的身份或异常字段使用空字符串，HTTP JSON 中使用 null。

~~~text
schema_version,record_id,batch_id,port,chip,efuse_id,efuse_field,mac,status,started_at,finished_at,duration_ms,firmware_sha256,identity_error,validation_error,logs
~~~

RecordResult.csv_path 表示实际持久化路径，upload_state 分别为 Disabled、SkippedIdentity、Uploaded、Failed、Deferred。CSV 内容不会随着 HTTP 结果被覆盖。队列提交返回错误时，调用者应保留原 RecordDraft；上传队列暂满的 Deferred 结果仍有已保存的 CSV。

引擎提供显式重试：RecordService.poll() 取出 Failed、Deferred 或本地保存失败的结果后，可再次调用 RecordService.submit(&settings, same_draft)。同一 RecordDraft 保留原 record_id/Idempotency-Key；已有 CSV 只有逐字节一致时才复用，内容不一致则拒绝覆盖和上传。服务在内存中对近期已成功处理的记录去重，服务端仍需持久化幂等键。

GUI 对本次运行保留的失败记录提供两类显式重试：

| 操作 | 使用的设置与记录 |
| --- | --- |
| “保存设置并重试”：本地 CSV 保存失败 | 保留原任务快照，使用当前保存的设置及 CSV 目录。 |
| “按当前设置重试上报”：CSV 已保存、HTTP Failed/Deferred | 要求当前上传已启用且 endpoint 合法；使用当前 endpoint/请求头，保留原 CSV 目录、任务快照及幂等键。 |

客户端不会在下次启动时自动扫描并重传历史 CSV。补发同一条业务记录时保留原记录和幂等键；不需要重新烧录或生成新的批次。

界面的“导出本批 CSV”是用户选择路径生成的批次汇总，包含原有的端口、芯片、状态、进度、起止时间、耗时、详情、固件及摘要，并追加批次 ID、eFuse 出厂 MAC 和 MAC。它与自动保存的逐记录 CSV 文件格式、保存时机不同；API 记录使用上面的 schema。

## 服务端联调示例

服务端准备好后，可使用保存为 record.json 的实际记录进行本机联调。以下命令不会替软件启用上传：

~~~sh
curl --request POST \
  --url http://127.0.0.1:8080/api/flash-records \
  --header 'Content-Type: application/json' \
  --header 'Idempotency-Key: record-8880d857e744ac27d8ae46954d2866f58620d6082fb8c6f5222eeae783029b7c' \
  --data-binary @record.json \
  --max-time 10 \
  --max-redirs 0
~~~

Python 服务端处理逻辑示意，存储事务和鉴权按业务系统实现：

~~~python
def receive_record(headers, record, store):
    if record.get("schema_version") != 1:
        return 400
    record_id = record.get("record_id")
    if not record_id or headers.get("Idempotency-Key") != record_id:
        return 400
    # insert_or_compare 是业务存储接口示意，需要事务或唯一约束。
    # 返回 inserted / same_record / conflict；重复同一记录也算成功。
    result = store.insert_or_compare(record_id, record)
    if result == "conflict":
        return 409
    return 204 if result in ("inserted", "same_record") else 500
~~~

业务服务端还应校验字段类型、身份格式、MAC 两种表示的一致性、允许的状态及正文大小。集成时以客户端发出的 schema_version 为版本判断依据，不从文件名或应用版本号推断 JSON 结构。
