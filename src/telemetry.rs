//! Durable per-device production records. Local persistence and optional network
//! delivery run on separate bounded workers; no GUI operation performs I/O.
use crate::{
    backend::DeviceIdentity,
    production::{PRODUCTION_HEADERS, ProductionBatch},
    telemetry_http,
};
use anyhow::{Context, Result, anyhow, ensure};
use regex::{Regex, RegexBuilder};
use serde::{
    Deserialize, Serialize,
    de::{MapAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    fmt,
    fs::{self, File},
    io::{Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MAX_LOG_BYTES: usize = 1024 * 1024;
const MAX_RECORD_BYTES: usize = 3 * 1024 * 1024;
const LOCAL_QUEUE_CAPACITY: usize = 64;
const UPLOAD_QUEUE_CAPACITY: usize = 32;
const RECENT_RECORDS: usize = 4096;
const MAX_ATTEMPTS: usize = 3;
const MAX_EXTRACTION_RULES: usize = 32;
const MAX_EXTRACTION_RULE_BYTES: usize = 32 * 1024;
const MAX_CUSTOM_VALUE_BYTES: usize = 4096;
pub const UNIQUE_DEVICE_ID_KEY: &str = "unique_device_id";
const MISSING_CAPTURE: &str = "本设备日志未找到第 1 个捕获组";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetrySettings {
    pub upload_enabled: bool,
    pub endpoint: String,
    pub timeout_seconds: u64,
    pub efuse_pattern: String,
    pub mac_pattern: String,
    #[serde(deserialize_with = "deserialize_extraction_rules")]
    pub extraction_rules: BTreeMap<String, String>,
    pub custom_headers: BTreeMap<String, String>,
    pub csv_directory: Option<PathBuf>,
}

// Configuration can contain authentication. Debug output must not reveal it.
impl fmt::Debug for TelemetrySettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelemetrySettings")
            .field("upload_enabled", &self.upload_enabled)
            .field("endpoint_configured", &!self.endpoint.is_empty())
            .field("timeout_seconds", &self.timeout_seconds)
            .field("efuse_pattern_bytes", &self.efuse_pattern.len())
            .field("mac_pattern_bytes", &self.mac_pattern.len())
            .field("extraction_rule_count", &self.extraction_rules.len())
            .field("custom_header_count", &self.custom_headers.len())
            .field("csv_directory", &self.csv_directory)
            .finish()
    }
}

impl Default for TelemetrySettings {
    fn default() -> Self {
        Self {
            upload_enabled: false,
            endpoint: String::new(),
            timeout_seconds: 10,
            efuse_pattern: r"EFUSE_ID=([0-9A-Fa-f]{12})".into(),
            mac_pattern: r"MAC=([0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5})".into(),
            extraction_rules: BTreeMap::new(),
            custom_headers: BTreeMap::new(),
            csv_directory: None,
        }
    }
}

pub fn validate_settings(settings: &TelemetrySettings) -> Result<()> {
    ensure!(
        (5..=30).contains(&settings.timeout_seconds),
        "上传超时必须为 5–30 秒"
    );
    ensure!(
        !settings.upload_enabled || !settings.endpoint.is_empty(),
        "启用上传前请填写服务器地址"
    );
    if !settings.endpoint.is_empty() {
        telemetry_http::validate_endpoint(&settings.endpoint)?;
    }
    telemetry_http::validate_custom_headers(&settings.custom_headers)?;
    compile_pattern(&settings.efuse_pattern, "eFuse ID")?;
    compile_pattern(&settings.mac_pattern, "MAC")?;
    validate_extraction_rules(&settings.extraction_rules)?;
    if let Some(directory) = &settings.csv_directory {
        ensure!(directory.is_absolute(), "CSV 目录必须是绝对路径");
    }
    Ok(())
}

fn compile_pattern(pattern: &str, label: &str) -> Result<Regex> {
    ensure!(
        !pattern.is_empty() && pattern.len() <= 4096,
        "{label} 提取规则必须为 1–4096 字节"
    );
    let regex = RegexBuilder::new(pattern)
        .size_limit(256 * 1024)
        .dfa_size_limit(1024 * 1024)
        .nest_limit(64)
        .build()
        .map_err(|_| anyhow!("{label} 提取规则不是有效且有界的正则表达式"))?;
    ensure!(
        regex.captures_len() >= 2,
        "{label} 提取规则必须包含第 1 个捕获组"
    );
    Ok(regex)
}

/// Parse the advanced editor without silently accepting duplicate field keys.
pub fn parse_extraction_rules_json(text: &str) -> Result<BTreeMap<String, String>> {
    ensure!(
        text.len() <= MAX_SETTINGS_BYTES as usize,
        "提取规则 JSON 超过 64 KiB"
    );
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let rules = deserialize_extraction_rules(&mut deserializer)
        .map_err(|_| anyhow!("提取规则 JSON 必须是没有重复键的字符串对象（最多 32 条）"))?;
    deserializer
        .end()
        .map_err(|_| anyhow!("提取规则 JSON 后存在多余内容"))?;
    validate_extraction_rules(&rules)?;
    Ok(rules)
}

fn deserialize_extraction_rules<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, String>, D::Error> {
    struct RulesVisitor;
    impl<'de> Visitor<'de> for RulesVisitor {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object mapping unique field keys to regex strings")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut rules = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if rules.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate extraction field key"));
                }
                if rules.len() > MAX_EXTRACTION_RULES {
                    return Err(serde::de::Error::custom("too many extraction fields"));
                }
            }
            Ok(rules)
        }
    }
    deserializer.deserialize_map(RulesVisitor)
}

pub fn validate_extraction_rules(rules: &BTreeMap<String, String>) -> Result<()> {
    CompiledExtractionRules::new(rules).map(|_| ())
}

/// One immutable matcher may be reused while a single device's UART is read.
#[derive(Debug)]
pub struct CompiledExtractionRules {
    rules: BTreeMap<String, Regex>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtractionOutcome {
    pub custom_fields: BTreeMap<String, String>,
    pub errors: BTreeMap<String, String>,
}

impl CompiledExtractionRules {
    pub fn new(rules: &BTreeMap<String, String>) -> Result<Self> {
        ensure!(
            rules.len() <= MAX_EXTRACTION_RULES,
            "自定义提取规则最多 32 条"
        );
        let mut compiled = BTreeMap::new();
        let mut total = 0usize;
        for (key, pattern) in rules {
            let mut bytes = key.bytes();
            ensure!(
                (1..=64).contains(&key.len())
                    && bytes
                        .next()
                        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
                    && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
                "自定义字段名必须为 1–64 字节 ASCII 字母、数字或下划线，且不能以数字开头"
            );
            total = total
                .checked_add(key.len() + pattern.len())
                .context("提取规则长度溢出")?;
            ensure!(
                total <= MAX_EXTRACTION_RULE_BYTES,
                "自定义提取规则总长度超过 32 KiB"
            );
            compiled.insert(
                key.clone(),
                compile_pattern(pattern, &format!("自定义字段 {key}"))?,
            );
        }
        Ok(Self { rules: compiled })
    }

    pub fn extract(&self, lines: &[String]) -> ExtractionOutcome {
        let too_large = lines
            .iter()
            .try_fold(0usize, |sum, line| {
                sum.checked_add(line.len())
                    .and_then(|sum| sum.checked_add(1))
            })
            .is_none_or(|bytes| bytes > MAX_LOG_BYTES);
        let text = if too_large {
            String::new()
        } else {
            lines.join("\n")
        };
        let mut outcome = ExtractionOutcome::default();
        for (key, regex) in &self.rules {
            let result = if too_large {
                Err(anyhow!("本设备日志超过 1 MiB"))
            } else {
                extract_custom_value(regex, &text)
            };
            match result {
                Ok(value) => {
                    outcome.custom_fields.insert(key.clone(), value);
                }
                Err(error) => {
                    outcome.errors.insert(key.clone(), error.to_string());
                }
            }
        }
        outcome
    }

    /// Recording/export boundary: credentials are removed before extraction,
    /// using the same redaction policy as persisted device logs.
    pub fn extract_record(
        &self,
        lines: &[String],
        headers: &BTreeMap<String, String>,
    ) -> ExtractionOutcome {
        if headers.is_empty() {
            self.extract(lines)
        } else {
            self.extract(&redact_header_credentials(lines.to_vec(), headers))
        }
    }
}

fn extract_custom_value(regex: &Regex, text: &str) -> Result<String> {
    let mut value: Option<&str> = None;
    for (index, captures) in regex.captures_iter(text).enumerate() {
        ensure!(index < 4096, "本设备字段匹配超过 4096 次");
        if let Some(capture) = captures.get(1) {
            let captured = capture.as_str();
            ensure!(
                !captured.trim().is_empty()
                    && captured.len() <= MAX_CUSTOM_VALUE_BYTES
                    && !captured.contains('\0'),
                "第 1 个捕获组为空、超过 4096 字节或含 NUL"
            );
            ensure!(
                !captured.contains("[redacted]"),
                "捕获值包含已脱敏的请求头凭据"
            );
            ensure!(
                value.is_none_or(|previous| previous == captured),
                "本设备日志包含多个不同的捕获值"
            );
            value = Some(captured);
        }
    }
    value.map(str::to_owned).context(MISSING_CAPTURE)
}

fn normalize_hardware_unique_id(value: &str) -> Result<String> {
    ensure!(
        value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "硬件 OPTIONAL_UNIQUE_ID 必须包含 32 个十六进制字符"
    );
    ensure!(
        !value.bytes().all(|byte| byte == b'0'),
        "硬件 OPTIONAL_UNIQUE_ID 未配置（全 0）"
    );
    Ok(value.to_ascii_uppercase())
}

/// Merge only an ID obtained from the backend's direct read-only hardware
/// field. UART text must use the normal regex extraction path instead.
/// Hardware wins; missing regex captures are fulfilled by it, while ambiguous
/// or contradictory regex captures remain explicit errors without leaking IDs.
pub fn apply_hardware_unique_id(outcome: &mut ExtractionOutcome, value: &str) -> Result<()> {
    let value = normalize_hardware_unique_id(value)?;
    let conflict = outcome
        .custom_fields
        .get(UNIQUE_DEVICE_ID_KEY)
        .is_some_and(|captured| !captured.eq_ignore_ascii_case(&value));
    let extraction_error = outcome
        .errors
        .get(UNIQUE_DEVICE_ID_KEY)
        .is_some_and(|error| error != MISSING_CAPTURE);
    outcome.errors.remove(UNIQUE_DEVICE_ID_KEY);
    if conflict || extraction_error {
        outcome.errors.insert(
            UNIQUE_DEVICE_ID_KEY.into(),
            "日志规则结果与硬件值冲突或无法确定唯一值；已采用硬件 OPTIONAL_UNIQUE_ID".into(),
        );
    }
    outcome
        .custom_fields
        .insert(UNIQUE_DEVICE_ID_KEY.into(), value);
    Ok(())
}

pub fn load_settings() -> Result<TelemetrySettings> {
    load_settings_from(&settings_path()?)
}

pub fn save_settings(settings: &TelemetrySettings) -> Result<()> {
    save_settings_at(&settings_path()?, settings)
}

pub fn load_settings_from(path: &Path) -> Result<TelemetrySettings> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TelemetrySettings::default());
        }
        Err(error) => return Err(error).context("无法读取记录与上传设置"),
    };
    let metadata = file.metadata().context("无法检查记录与上传设置")?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SETTINGS_BYTES,
        "记录与上传设置无效或超过 64 KiB"
    );
    // A malformed serde value can contain credentials; do not expose its text.
    let settings: TelemetrySettings = serde_json::from_reader(file.take(MAX_SETTINGS_BYTES + 1))
        .map_err(|_| anyhow!("记录与上传设置的 JSON 格式无效"))?;
    validate_settings(&settings)?;
    Ok(settings)
}

pub fn save_settings_at(path: &Path, settings: &TelemetrySettings) -> Result<()> {
    validate_settings(settings)?;
    let bytes = serde_json::to_vec_pretty(settings).context("无法编码记录与上传设置")?;
    ensure!(
        bytes.len() < MAX_SETTINGS_BYTES as usize,
        "记录与上传设置超过 64 KiB"
    );
    let directory = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).context("无法创建记录与上传设置目录")?;
    let mut file =
        tempfile::NamedTempFile::new_in(directory).context("无法创建私有设置临时文件")?;
    file.write_all(&bytes).context("无法写入记录与上传设置")?;
    file.write_all(b"\n").context("无法写入记录与上传设置")?;
    file.as_file()
        .sync_all()
        .context("无法持久化记录与上传设置")?;
    file.persist(path)
        .map_err(|error| error.error)
        .context("无法原子替换记录与上传设置")?;
    sync_directory(directory)?;
    Ok(())
}

fn settings_path() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("Windows 未提供 LOCALAPPDATA")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|value| PathBuf::from(value).join(".config")))
        .context("无法定位当前用户的设置目录")?;
    Ok(base.join("FlashTool").join("telemetry.json"))
}

pub fn default_records_directory() -> Result<PathBuf> {
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .context("Windows 未提供 LOCALAPPDATA")?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|value| PathBuf::from(value).join(".local/share")))
        .context("无法定位当前用户的数据目录")?;
    Ok(base.join("FlashTool").join("records"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Success,
    Failed,
    Cancelled,
}

impl RecordStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordOperation {
    #[default]
    Flash,
    ReadOnly,
}

impl RecordOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Flash => "flash",
            Self::ReadOnly => "read_only",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RecordDraft {
    pub batch_id: String,
    pub port: String,
    pub chip: String,
    pub identity: Option<DeviceIdentity>,
    pub status: RecordStatus,
    pub started_at: String,
    pub finished_at: String,
    pub duration_ms: u64,
    pub firmware_sha256: String,
    pub logs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadState {
    Disabled,
    SkippedIdentity,
    Uploaded,
    Failed,
    Deferred,
}

#[derive(Clone, Debug)]
pub struct RecordResult {
    pub record_id: String,
    pub batch_id: String,
    pub port: String,
    pub csv_path: Option<PathBuf>,
    pub upload_state: UploadState,
    pub error: Option<String>,
    pub operation: RecordOperation,
    pub custom_fields: BTreeMap<String, String>,
    pub custom_field_errors: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
struct DeviceRecord {
    schema_version: u32,
    record_id: String,
    batch_id: String,
    port: String,
    chip: String,
    efuse_id: Option<String>,
    efuse_field: &'static str,
    mac: Option<String>,
    status: RecordStatus,
    started_at: String,
    finished_at: String,
    duration_ms: u64,
    firmware_sha256: String,
    logs: Vec<String>,
    identity_error: Option<String>,
    validation_error: Option<String>,
    operation: RecordOperation,
    custom_fields: BTreeMap<String, String>,
    custom_field_errors: BTreeMap<String, String>,
    // Production metadata belongs to local CSV only; the HTTP contract stays
    // unchanged and does not require ProductionBatch to implement Serialize.
    #[serde(skip)]
    production: Option<ProductionMetadata>,
}

#[derive(Clone, Debug)]
struct ProductionMetadata {
    work_order: String,
    board_week: String,
    board_date: String,
}

impl From<&ProductionBatch> for ProductionMetadata {
    fn from(production: &ProductionBatch) -> Self {
        Self {
            work_order: production.work_order.clone(),
            board_week: production.board_week.clone(),
            board_date: production.board_date.to_string(),
        }
    }
}

static NEXT_BATCH: AtomicU64 = AtomicU64::new(0);

pub fn new_batch_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let count = NEXT_BATCH.fetch_add(1, Ordering::Relaxed);
    let mut hash = Sha256::new();
    hash.update(b"FlashTool batch v1\0");
    hash.update(now.to_le_bytes());
    hash.update(std::process::id().to_le_bytes());
    hash.update(count.to_le_bytes());
    format!("batch-{:x}", hash.finalize())
}

fn record_id(draft: &RecordDraft, operation: RecordOperation) -> String {
    let mut hash = Sha256::new();
    hash.update(match operation {
        RecordOperation::Flash => b"FlashTool record v1\0".as_slice(),
        RecordOperation::ReadOnly => b"FlashTool read-only record v1\0".as_slice(),
    });
    hash.update((draft.batch_id.len() as u64).to_le_bytes());
    hash.update(draft.batch_id.as_bytes());
    hash.update(draft.port.as_bytes());
    format!("record-{:x}", hash.finalize())
}

struct LocalTask {
    settings: TelemetrySettings,
    draft: RecordDraft,
    record_id: String,
    operation: RecordOperation,
    hardware_unique_device_id: Option<String>,
    production: Option<ProductionMetadata>,
}
struct UploadTask {
    settings: TelemetrySettings,
    record: DeviceRecord,
    csv_path: PathBuf,
}

type Wake = dyn Fn() + Send + Sync;

pub struct RecordService {
    sender: SyncSender<LocalTask>,
    receiver: Receiver<RecordResult>,
    local_pending: Arc<AtomicUsize>,
    pending: HashSet<String>,
    accepted: HashSet<String>,
    recent: VecDeque<String>,
    startup_error: Option<String>,
}

impl RecordService {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (sender, local_receiver) = mpsc::sync_channel::<LocalTask>(LOCAL_QUEUE_CAPACITY);
        let (upload_sender, upload_receiver) =
            mpsc::sync_channel::<UploadTask>(UPLOAD_QUEUE_CAPACITY);
        let (results, receiver) = mpsc::channel();
        let local_pending = Arc::new(AtomicUsize::new(0));
        let wake: Arc<Wake> = Arc::new(wake);
        let upload_results = results.clone();
        let upload_wake = wake.clone();
        // If this worker cannot start, its closed receiver is handled by the
        // local worker as Deferred after CSV is already durable.
        let _upload_worker = thread::Builder::new()
            .name("flashtool-upload".into())
            .spawn(move || {
                while let Ok(task) = upload_receiver.recv() {
                    let fallback = result_for(
                        &task.record,
                        Some(task.csv_path.clone()),
                        UploadState::Failed,
                        Some("上传工作线程出现异常；本地 CSV 已保存".into()),
                    );
                    let result =
                        catch_unwind(AssertUnwindSafe(|| upload_record(task))).unwrap_or(fallback);
                    deliver(&upload_results, &upload_wake, result);
                }
            });
        let pending = local_pending.clone();
        let local_worker = thread::Builder::new()
            .name("flashtool-record-csv".into())
            .spawn(move || {
                while let Ok(task) = local_receiver.recv() {
                    let fallback = RecordResult {
                        record_id: task.record_id.clone(),
                        batch_id: task.draft.batch_id.clone(),
                        port: task.draft.port.clone(),
                        csv_path: None,
                        upload_state: UploadState::Failed,
                        error: Some("本地记录工作线程出现异常；未进行上传".into()),
                        operation: task.operation,
                        custom_fields: BTreeMap::new(),
                        custom_field_errors: BTreeMap::new(),
                    };
                    let outcome = catch_unwind(AssertUnwindSafe(|| persist_task(task)));
                    match outcome {
                        Ok(Ok(LocalOutcome::Complete(result))) => deliver(&results, &wake, result),
                        Ok(Ok(LocalOutcome::Upload(task))) => {
                            if let Err(error) = upload_sender.try_send(*task) {
                                let task = match error {
                                    TrySendError::Full(task) | TrySendError::Disconnected(task) => {
                                        task
                                    }
                                };
                                let result = result_for(
                                    &task.record,
                                    Some(task.csv_path),
                                    UploadState::Deferred,
                                    Some(
                                        "本地 CSV 已保存；上传队列已满或不可用，未发送此记录"
                                            .into(),
                                    ),
                                );
                                deliver(&results, &wake, result);
                            }
                        }
                        Ok(Err(error)) => deliver(
                            &results,
                            &wake,
                            RecordResult {
                                error: Some(format!("本地 CSV 保存失败，未进行上传：{error:#}")),
                                ..fallback
                            },
                        ),
                        Err(_) => deliver(&results, &wake, fallback),
                    }
                    pending.fetch_sub(1, Ordering::AcqRel);
                    wake_safely(&wake);
                }
            });
        Self {
            sender,
            receiver,
            local_pending,
            pending: HashSet::new(),
            accepted: HashSet::new(),
            recent: VecDeque::new(),
            startup_error: local_worker
                .err()
                .map(|_| "无法启动本地 CSV 保存线程".into()),
        }
    }

    /// Snapshot settings; enqueue without touching the filesystem/network or
    /// blocking the UI. An Err leaves the caller responsible for retaining and
    /// retrying its draft. Duplicate successful submissions are rejected.
    pub fn submit(&mut self, settings: &TelemetrySettings, draft: RecordDraft) -> Result<String> {
        self.submit_with_operation(settings, draft, RecordOperation::Flash)
    }

    pub fn submit_with_operation(
        &mut self,
        settings: &TelemetrySettings,
        draft: RecordDraft,
        operation: RecordOperation,
    ) -> Result<String> {
        self.submit_with_identity(settings, draft, operation, None)
    }

    /// The optional ID must come from the direct hardware progress event, not
    /// from UART text or a reformatted factory MAC. Old callers retain their
    /// existing regex behavior by omitting this argument.
    pub fn submit_with_identity(
        &mut self,
        settings: &TelemetrySettings,
        draft: RecordDraft,
        operation: RecordOperation,
        unique_device_id: Option<&str>,
    ) -> Result<String> {
        self.submit_with_production(settings, draft, operation, unique_device_id, None)
    }

    /// The caller freezes both the resolved CSV directory and production
    /// metadata for the job. Retries must retain the same directory/metadata;
    /// this layer never appends the production directory a second time.
    pub fn submit_with_production(
        &mut self,
        settings: &TelemetrySettings,
        draft: RecordDraft,
        operation: RecordOperation,
        unique_device_id: Option<&str>,
        production: Option<&ProductionBatch>,
    ) -> Result<String> {
        let hardware_unique_device_id = unique_device_id
            .map(normalize_hardware_unique_id)
            .transpose()?;
        ensure!(self.startup_error.is_none(), "无法启动本地 CSV 保存线程");
        ensure!(
            self.pending.len() < LOCAL_QUEUE_CAPACITY,
            "记录后台队列已满，请保留记录并稍后重试"
        );
        ensure!(
            !draft.batch_id.is_empty()
                && draft.batch_id.len() <= 256
                && !draft.port.is_empty()
                && draft.port.len() <= 256,
            "批次或串口标识无效"
        );
        ensure!(
            draft.chip.len() <= 128
                && draft.started_at.len() <= 128
                && draft.finished_at.len() <= 128
                && draft.firmware_sha256.len() <= 128,
            "记录字段超过长度限制"
        );
        let bytes = draft
            .logs
            .iter()
            .try_fold(0usize, |sum, line| {
                sum.checked_add(line.len())
                    .and_then(|value| value.checked_add(1))
            })
            .unwrap_or(usize::MAX);
        ensure!(
            bytes <= MAX_LOG_BYTES,
            "单条设备记录日志超过 1 MiB，请保留记录并缩短日志后重试"
        );
        let id = record_id(&draft, operation);
        ensure!(
            !self.accepted.contains(&id),
            "同一批次和串口的记录已经提交，未重复保存或上传"
        );
        self.local_pending.fetch_add(1, Ordering::AcqRel);
        let task = LocalTask {
            settings: settings.clone(),
            draft,
            record_id: id.clone(),
            operation,
            hardware_unique_device_id,
            production: production.map(ProductionMetadata::from),
        };
        if self.sender.try_send(task).is_err() {
            self.local_pending.fetch_sub(1, Ordering::AcqRel);
            return Err(anyhow!("本地记录队列已满或不可用，请保留记录并稍后重试"));
        }
        self.pending.insert(id.clone());
        self.accepted.insert(id.clone());
        self.recent.push_back(id.clone());
        let mut attempts = self.recent.len();
        while self.accepted.len() > RECENT_RECORDS && attempts > 0 {
            attempts -= 1;
            if let Some(old) = self.recent.pop_front() {
                if self.pending.contains(&old) {
                    self.recent.push_back(old);
                } else {
                    self.accepted.remove(&old);
                }
            }
        }
        Ok(id)
    }

    pub fn poll(&mut self) -> Option<RecordResult> {
        let result = self.receiver.try_recv().ok()?;
        self.pending.remove(&result.record_id);
        // A visible failure may be explicitly resubmitted with the same stable
        // idempotency key, e.g. after selecting a writable local CSV directory.
        if result.csv_path.is_none()
            || matches!(
                result.upload_state,
                UploadState::Failed | UploadState::Deferred
            )
        {
            self.accepted.remove(&result.record_id);
            self.recent.retain(|id| id != &result.record_id);
        }
        Some(result)
    }

    /// Includes final results not yet consumed by the GUI.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    /// Used by close protection; HTTP timeouts do not hold local CSV hostage.
    pub fn pending_local_count(&self) -> usize {
        self.local_pending.load(Ordering::Acquire)
    }
}

fn wake_safely(wake: &Arc<Wake>) {
    let _ = catch_unwind(AssertUnwindSafe(|| wake()));
}
fn deliver(sender: &mpsc::Sender<RecordResult>, wake: &Arc<Wake>, result: RecordResult) {
    if sender.send(result).is_ok() {
        wake_safely(wake);
    }
}

enum LocalOutcome {
    Complete(RecordResult),
    Upload(Box<UploadTask>),
}

fn persist_task(task: LocalTask) -> Result<LocalOutcome> {
    let record = prepare_record(
        task.record_id,
        task.draft,
        &task.settings,
        task.operation,
        task.hardware_unique_device_id.as_deref(),
        task.production,
    );
    let directory = match &task.settings.csv_directory {
        Some(path) => path.clone(),
        None => default_records_directory()?,
    };
    let csv_path = persist_csv(&directory, &record)?;
    if !task.settings.upload_enabled {
        return Ok(LocalOutcome::Complete(result_for(
            &record,
            Some(csv_path),
            UploadState::Disabled,
            None,
        )));
    }
    if let Some(reason) = &record.identity_error {
        return Ok(LocalOutcome::Complete(result_for(
            &record,
            Some(csv_path),
            UploadState::SkippedIdentity,
            Some(format!("本地 CSV 已保存；设备身份未验证，未上传：{reason}")),
        )));
    }
    if let Some(reason) = &record.validation_error {
        return Ok(LocalOutcome::Complete(result_for(
            &record,
            Some(csv_path),
            UploadState::Failed,
            Some(format!("本地 CSV 已保存；记录校验失败，未上传：{reason}")),
        )));
    }
    if validate_settings(&task.settings).is_err() {
        return Ok(LocalOutcome::Complete(result_for(
            &record,
            Some(csv_path),
            UploadState::Failed,
            Some("本地 CSV 已保存；上传设置无效，未发送记录".into()),
        )));
    }
    Ok(LocalOutcome::Upload(Box::new(UploadTask {
        settings: task.settings,
        record,
        csv_path,
    })))
}

fn prepare_record(
    id: String,
    draft: RecordDraft,
    settings: &TelemetrySettings,
    operation: RecordOperation,
    hardware_unique_device_id: Option<&str>,
    production: Option<ProductionMetadata>,
) -> DeviceRecord {
    let logs = redact_header_credentials(draft.logs.clone(), &settings.custom_headers);
    let mut extracted = match CompiledExtractionRules::new(&settings.extraction_rules) {
        Ok(rules) => rules.extract(&logs),
        Err(error) => ExtractionOutcome {
            custom_fields: BTreeMap::new(),
            errors: BTreeMap::from([("_rules".into(), error.to_string())]),
        },
    };
    if let Some(value) = hardware_unique_device_id
        && let Err(error) = apply_hardware_unique_id(&mut extracted, value)
    {
        extracted.custom_fields.remove(UNIQUE_DEVICE_ID_KEY);
        extracted
            .errors
            .insert(UNIQUE_DEVICE_ID_KEY.into(), error.to_string());
    }
    let identity = if operation == RecordOperation::ReadOnly && draft.identity.is_none() {
        Err(anyhow!("本次只读日志未提取到无歧义的自定义字段"))
    } else {
        verified_identity(&draft, settings)
    };
    let (efuse_id, mac, identity_error) = match identity {
        Ok((efuse, mac)) => (Some(efuse), Some(mac), None),
        Err(_) if operation == RecordOperation::ReadOnly && !extracted.custom_fields.is_empty() => {
            (None, None, None)
        }
        Err(error) => (None, None, Some(error.to_string())),
    };
    let validation_error = if chrono::DateTime::parse_from_rfc3339(&draft.started_at).is_err()
        || chrono::DateTime::parse_from_rfc3339(&draft.finished_at).is_err()
    {
        Some("起止时间不是有效的 RFC3339 时间".into())
    } else if operation == RecordOperation::Flash
        && (draft.firmware_sha256.len() != 64
            || !draft
                .firmware_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()))
    {
        Some("固件 SHA256 缺失或格式无效".into())
    } else if operation == RecordOperation::ReadOnly && draft.status != RecordStatus::Success {
        Some("只读任务未成功完成，仅保存本地记录".into())
    } else if operation == RecordOperation::ReadOnly && extracted.custom_fields.is_empty() {
        Some("只读任务未提取到无歧义的自定义字段，仅保存本地记录".into())
    } else {
        None
    };
    DeviceRecord {
        schema_version: 1,
        record_id: id,
        batch_id: draft.batch_id,
        port: draft.port,
        chip: draft.chip,
        efuse_id,
        efuse_field: "factory_mac",
        mac,
        status: draft.status,
        started_at: draft.started_at,
        finished_at: draft.finished_at,
        duration_ms: draft.duration_ms,
        firmware_sha256: draft.firmware_sha256.to_ascii_lowercase(),
        logs,
        identity_error,
        validation_error,
        operation,
        custom_fields: extracted.custom_fields,
        custom_field_errors: extracted.errors,
        production,
    }
}

fn verified_identity(
    draft: &RecordDraft,
    settings: &TelemetrySettings,
) -> Result<(String, String)> {
    let typed = draft
        .identity
        .as_ref()
        .context("本任务未读取到芯片的只读身份")?;
    let typed_efuse = normalize_efuse(
        typed
            .efuse_id
            .as_deref()
            .context("芯片未提供工厂 MAC eFuse ID")?,
    )?;
    let typed_mac = normalize_mac(typed.mac.as_deref().context("芯片未提供 MAC 地址")?)?;
    ensure!(
        typed_efuse == typed_mac.replace(':', ""),
        "芯片 eFuse ID 与工厂 MAC 不一致"
    );
    let text = draft.logs.join("\n");
    let efuse_regex = compile_pattern(&settings.efuse_pattern, "eFuse ID")?;
    let mac_regex = compile_pattern(&settings.mac_pattern, "MAC")?;
    let efuse = extract_unique(&efuse_regex, &text, normalize_efuse, "eFuse ID")?;
    let mac = extract_unique(&mac_regex, &text, normalize_mac, "MAC")?;
    ensure!(
        efuse == typed_efuse && mac == typed_mac,
        "本任务日志中的 ID/MAC 与芯片只读身份不一致"
    );
    Ok((efuse, mac))
}

fn extract_unique(
    regex: &Regex,
    logs: &str,
    normalize: fn(&str) -> Result<String>,
    label: &str,
) -> Result<String> {
    let mut values = BTreeSet::new();
    for (index, captures) in regex.captures_iter(logs).enumerate() {
        ensure!(index < 4096, "{label} 日志匹配过多，无法确定设备身份");
        if let Some(value) = captures.get(1) {
            values.insert(normalize(value.as_str())?);
            ensure!(
                values.len() <= 1,
                "本任务日志包含多个不同的 {label}，已阻止绑定和上传"
            );
        }
    }
    values
        .into_iter()
        .next()
        .with_context(|| format!("本任务日志未找到 {label} 的第 1 个捕获组"))
}

fn normalize_efuse(value: &str) -> Result<String> {
    ensure!(
        value.len() == 12 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "eFuse ID 必须为 12 位十六进制工厂 MAC"
    );
    let normalized = value.to_ascii_uppercase();
    ensure!(
        normalized != "000000000000" && normalized != "FFFFFFFFFFFF",
        "eFuse ID 是无效的空白值"
    );
    Ok(normalized)
}

fn normalize_mac(value: &str) -> Result<String> {
    let parts: Vec<_> = value.split(':').collect();
    ensure!(
        parts.len() == 6
            && parts
                .iter()
                .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "MAC 必须为六组冒号分隔的十六进制字节"
    );
    let normalized = parts.join(":").to_ascii_uppercase();
    ensure!(
        normalized != "00:00:00:00:00:00" && normalized != "FF:FF:FF:FF:FF:FF",
        "MAC 是无效的空白值"
    );
    Ok(normalized)
}

fn redact_header_credentials(
    mut logs: Vec<String>,
    headers: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut values: Vec<&str> = headers
        .values()
        .map(String::as_str)
        .filter(|value| value.len() >= 4)
        .collect();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization")
            && let Some((_, token)) = value.split_once(' ')
            && token.len() >= 4
        {
            values.push(token);
        }
    }
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    for line in &mut logs {
        for value in &values {
            *line = line.replace(value, "[redacted]");
        }
    }
    logs
}

fn csv_cell(value: &str) -> String {
    encode_csv_cell(value, false)
}

fn encode_csv_cell(value: &str, protect_apostrophe: bool) -> String {
    let first = value
        .trim_start_matches(|character: char| {
            character.is_whitespace() || character.is_control() || character == '\u{feff}'
        })
        .chars()
        .next();
    let formula = first.is_some_and(|character| {
        matches!(character, '=' | '+' | '-' | '@') || (protect_apostrophe && character == '\'')
    }) || value.starts_with(['\t', '\r', '\n']);
    format!(
        "\"{}{}\"",
        if formula { "'" } else { "" },
        value.replace('"', "\"\"")
    )
}

fn csv_bytes(record: &DeviceRecord) -> Vec<u8> {
    csv_bytes_for_format(record, true, true)
}

// The previous format is rendered only to compare an existing immutable file
// during a retry. Existing records are never rewritten to add a new column.
fn csv_bytes_for_format(
    record: &DeviceRecord,
    include_unique_device_id: bool,
    include_production: bool,
) -> Vec<u8> {
    let mut header = vec![
        "schema_version",
        "record_id",
        "batch_id",
        "port",
        "chip",
        "efuse_id",
        "efuse_field",
        "mac",
        "status",
        "started_at",
        "finished_at",
        "duration_ms",
        "firmware_sha256",
        "identity_error",
        "validation_error",
        "logs",
        "operation",
        "custom_fields",
        "custom_field_errors",
    ];
    let mut values = vec![
        record.schema_version.to_string(),
        record.record_id.clone(),
        record.batch_id.clone(),
        record.port.clone(),
        record.chip.clone(),
        record.efuse_id.clone().unwrap_or_default(),
        record.efuse_field.to_owned(),
        record.mac.clone().unwrap_or_default(),
        record.status.as_str().to_owned(),
        record.started_at.clone(),
        record.finished_at.clone(),
        record.duration_ms.to_string(),
        record.firmware_sha256.clone(),
        record.identity_error.clone().unwrap_or_default(),
        record.validation_error.clone().unwrap_or_default(),
        record.logs.join("\n"),
        record.operation.as_str().to_owned(),
        serde_json::to_string(&record.custom_fields).expect("string maps are valid JSON"),
        serde_json::to_string(&record.custom_field_errors).expect("string maps are valid JSON"),
    ];
    if include_unique_device_id {
        header.push(UNIQUE_DEVICE_ID_KEY);
        values.push(
            record
                .custom_fields
                .get(UNIQUE_DEVICE_ID_KEY)
                .cloned()
                .unwrap_or_default(),
        );
    }
    if include_production {
        header.extend(PRODUCTION_HEADERS);
        values.extend([
            record
                .production
                .as_ref()
                .map_or("", |production| production.work_order.as_str())
                .to_owned(),
            record
                .production
                .as_ref()
                .map_or("", |production| production.board_week.as_str())
                .to_owned(),
            record
                .production
                .as_ref()
                .map_or("", |production| production.board_date.as_str())
                .to_owned(),
        ]);
    }
    let mut output = vec![0xef, 0xbb, 0xbf];
    output.extend_from_slice(
        header
            .iter()
            .map(|value| csv_cell(value))
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                encode_csv_cell(value, include_production && index >= values.len() - 3)
            })
            .collect::<Vec<_>>()
            .join(",")
            .as_bytes(),
    );
    output.extend_from_slice(b"\r\n");
    output
}

fn persist_csv(directory: &Path, record: &DeviceRecord) -> Result<PathBuf> {
    ensure!(directory.is_absolute(), "CSV 目录必须是绝对路径");
    fs::create_dir_all(directory).context("无法创建 CSV 目录")?;
    let path = directory.join(format!("{}.csv", record.record_id));
    let bytes = csv_bytes(record);
    ensure!(
        bytes.len() <= MAX_RECORD_BYTES,
        "本地 CSV 记录超过安全大小限制"
    );
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).context("无法创建本地 CSV 临时文件")?;
    temporary.write_all(&bytes).context("无法写入本地 CSV")?;
    temporary
        .as_file()
        .sync_all()
        .context("无法持久化本地 CSV")?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut old = Vec::new();
            File::open(&path)
                .context("无法校验已有本地 CSV")?
                .take((MAX_RECORD_BYTES + 1) as u64)
                .read_to_end(&mut old)
                .context("无法读取已有本地 CSV")?;
            ensure!(
                old == bytes
                    || (record.production.is_none()
                        && (old == csv_bytes_for_format(record, true, false)
                            || old == csv_bytes_for_format(record, false, false))),
                "同一批次和串口已存在内容不同的 CSV，已拒绝覆盖和上传"
            );
        }
        Err(error) => return Err(error.error).context("无法保存本地 CSV"),
    }
    sync_directory(directory)?;
    Ok(path)
}

fn sync_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(directory)
        .context("无法打开记录目录进行持久化")?
        .sync_all()
        .context("无法持久化记录目录")?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn result_for(
    record: &DeviceRecord,
    csv_path: Option<PathBuf>,
    upload_state: UploadState,
    error: Option<String>,
) -> RecordResult {
    RecordResult {
        record_id: record.record_id.clone(),
        batch_id: record.batch_id.clone(),
        port: record.port.clone(),
        csv_path,
        upload_state,
        error,
        operation: record.operation,
        custom_fields: record.custom_fields.clone(),
        custom_field_errors: record.custom_field_errors.clone(),
    }
}

fn upload_record(task: UploadTask) -> RecordResult {
    let body = match serde_json::to_vec(&task.record) {
        Ok(body) => body,
        Err(_) => {
            return result_for(
                &task.record,
                Some(task.csv_path),
                UploadState::Failed,
                Some("本地 CSV 已保存；无法编码上传 JSON".into()),
            );
        }
    };
    let mut last_error = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let retryable = match telemetry_http::post_json(
            &task.settings.endpoint,
            task.settings.timeout_seconds,
            &task.settings.custom_headers,
            &task.record.record_id,
            &body,
        ) {
            Ok(response) if (200..300).contains(&response.status) => {
                return result_for(
                    &task.record,
                    Some(task.csv_path),
                    UploadState::Uploaded,
                    None,
                );
            }
            Ok(response) => {
                last_error = format!("服务器返回 HTTP {}", response.status);
                matches!(response.status, 408 | 425 | 429 | 500 | 502 | 503 | 504)
            }
            Err(error) => {
                last_error = error.message;
                error.retryable
            }
        };
        if !retryable || attempt + 1 == MAX_ATTEMPTS {
            break;
        }
        thread::sleep(Duration::from_millis(100 * (attempt as u64 + 1)));
    }
    result_for(
        &task.record,
        Some(task.csv_path),
        UploadState::Failed,
        Some(format!("本地 CSV 已保存；上传失败：{last_error}")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn csv_formula_protection_handles_leading_nul_and_preserves_normal_ids() {
        for value in ["\0=1+1", "\u{1f} @SUM(A1)", "\u{feff}\0-1", "\t+1"] {
            assert!(
                csv_cell(value).starts_with("\"'"),
                "control-prefixed formula was not neutralized"
            );
        }
        assert_eq!(csv_cell("AABBCCDDEEFF"), "\"AABBCCDDEEFF\"");
        assert_eq!(csv_cell("AA:BB:CC:DD:EE:FF"), "\"AA:BB:CC:DD:EE:FF\"");
    }

    #[test]
    fn repeated_local_failures_can_be_retried_without_growing_dedup_history() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        fs::write(&blocked, "preserve existing file").unwrap();
        let mut settings = TelemetrySettings {
            csv_directory: Some(blocked),
            ..TelemetrySettings::default()
        };
        let draft = RecordDraft {
            batch_id: "retry-local-record".into(),
            port: "COM3".into(),
            chip: "esp32c3".into(),
            identity: None,
            status: RecordStatus::Cancelled,
            started_at: "2026-11-19T00:00:00Z".into(),
            finished_at: "2026-11-19T00:00:01Z".into(),
            duration_ms: 1000,
            firmware_sha256: "a".repeat(64),
            logs: vec!["Cancelled before connecting".into()],
        };
        let mut service = RecordService::new(|| {});
        let mut stable_id = None;
        for _ in 0..12 {
            let id = service.submit(&settings, draft.clone()).unwrap();
            assert!(stable_id.as_ref().is_none_or(|previous| previous == &id));
            stable_id = Some(id);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(result) = service.poll() {
                    assert!(result.csv_path.is_none() && result.error.is_some());
                    break;
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(service.accepted.is_empty());
            assert!(
                service.recent.is_empty(),
                "failed retries retained stale dedup history"
            );
        }
        settings.csv_directory = Some(directory.path().join("writable"));
        assert_eq!(
            service.submit(&settings, draft.clone()).unwrap(),
            stable_id.unwrap()
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(result) = service.poll() {
                assert_eq!(result.upload_state, UploadState::Disabled);
                assert!(result.csv_path.unwrap().is_file());
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            service.submit(&settings, draft).is_err(),
            "successful records must still be deduplicated"
        );
    }
}
