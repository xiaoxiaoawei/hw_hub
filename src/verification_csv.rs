//! Shared, append-only verification history, committed by atomic replacement.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};

use crate::{production::ProductionBatch, verification::VerificationChecks};

const NAME: &str = "validation-results.csv";
const BOM: &[u8] = b"\xef\xbb\xbf";
const MAX_ROW_BYTES: usize = 2 * 1024 * 1024;
const HEADER: [&str; 10] = [
    "batch",
    "COM",
    "time",
    "MAX30105",
    "BMI270",
    "MAC",
    "unique_device_id",
    "battery_mv",
    "result",
    "failures",
];
const PRODUCTION_COLUMNS: [&str; 4] =
    ["生产批次/工单号", "主板年周", "主板生产日期", "startup_log"];
#[cfg(test)]
const MAX_COLUMNS: usize = HEADER.len() + PRODUCTION_COLUMNS.len();

#[derive(Clone, Debug)]
pub struct VerificationRecord {
    pub batch_id: String,
    pub port: String,
    pub finished_at: String,
    pub checks: VerificationChecks,
    pub startup_log: String,
}

/// Preserve the legacy ten-column history format. Production metadata and
/// startup logs belong in the separate per-work-order format below.
pub fn append_batch(directory: &Path, records: &[VerificationRecord]) -> Result<PathBuf> {
    append_records(directory, records, NAME, None, false)
}

/// Append full frozen captures to a deterministic CSV for this work order.
/// The supplied directory is already routed by the caller. The timestamp is
/// preserved as supplied, including the caller's Beijing +08:00 offset.
pub fn append_batch_with_production(
    directory: &Path,
    records: &[VerificationRecord],
    production: &ProductionBatch,
) -> Result<PathBuf> {
    append_batch_with_optional_production(directory, records, Some(production))
}

/// Preserve the full capture even when any or all production fields are blank.
/// The default name deliberately differs from the legacy ten-column history.
pub fn append_batch_with_optional_production(
    directory: &Path,
    records: &[VerificationRecord],
    production: Option<&ProductionBatch>,
) -> Result<PathBuf> {
    let production = production
        .map(|value| {
            ProductionBatch::parse(
                &value.work_order,
                &value.board_week,
                &value.board_date_text(),
            )
        })
        .transpose()?;
    let name = match production
        .as_ref()
        .filter(|value| !value.work_order.is_empty())
    {
        Some(production) => format!(
            "validation-{:x}.csv",
            Sha256::digest(production.work_order.as_bytes())
        ),
        None => "validation-records.csv".into(),
    };
    append_records(directory, records, &name, production.as_ref(), true)
}

/// Save a complete batch snapshot. Repeating identical rows is a no-op;
/// changing a previously stored batch/COM row is rejected without overwriting.
/// All cooperating writers must use this persistent sidecar lock.
fn append_records(
    directory: &Path,
    records: &[VerificationRecord],
    name: &str,
    production: Option<&ProductionBatch>,
    include_capture: bool,
) -> Result<PathBuf> {
    ensure!(!records.is_empty(), "没有待保存的验证结果");
    let batch = &records[0].batch_id;
    ensure!(!batch.is_empty(), "验证批次标识不能为空");
    let mut header = HEADER.to_vec();
    if include_capture {
        header.extend(PRODUCTION_COLUMNS);
    }
    let mut rows = Vec::with_capacity(records.len());
    for record in records {
        ensure!(&record.batch_id == batch, "一次只能保存同一验证批次");
        ensure!(!record.port.is_empty(), "验证端口不能为空");
        let mut cells = record_cells(record);
        if include_capture {
            cells.extend([
                production.map_or_else(String::new, |value| value.work_order.clone()),
                production.map_or_else(String::new, |value| value.board_week.clone()),
                production.map_or_else(String::new, ProductionBatch::board_date_text),
                record.startup_log.clone(),
            ]);
        }
        rows.push(cells);
    }
    // Preserve the original complete batch/COM snapshot contract. BLE history
    // uses the same storage without this group constraint, one attempt at a time.
    append_rows(directory, name, &header, &rows, &[0, 1], Some(0))
}

/// Shared atomic CSV history. `rows` are raw strings; escaping and spreadsheet
/// protection happen once here. Existing values and headers remain immutable.
/// A complete group requires every existing key in an incoming group to be
/// represented in this snapshot (the original device-verification contract).
pub(crate) fn append_rows(
    directory: &Path,
    name: &str,
    header: &[&str],
    rows: &[Vec<String>],
    key_columns: &[usize],
    complete_group: Option<usize>,
) -> Result<PathBuf> {
    ensure!(directory.is_absolute(), "验证记录目录必须为绝对路径");
    ensure!(!rows.is_empty(), "没有待保存的验证结果");
    ensure!(
        Path::new(name)
            .file_name()
            .is_some_and(|value| value == name)
            && !name.contains(['\\', ':'])
            && name.ends_with(".csv"),
        "验证记录文件名无效"
    );
    ensure!(
        !header.is_empty()
            && header.len() <= 64
            && !key_columns.is_empty()
            && key_columns.iter().all(|index| *index < header.len())
            && complete_group.is_none_or(|index| index < header.len()),
        "验证记录列定义无效"
    );
    let key = |cells: &[String]| {
        key_columns
            .iter()
            .map(|index| cells[*index].clone())
            .collect::<Vec<_>>()
    };
    let mut expected = BTreeMap::new();
    let mut incoming = Vec::with_capacity(rows.len());
    let mut groups = BTreeSet::new();
    for raw in rows {
        ensure!(raw.len() == header.len(), "待保存的验证记录列数不匹配");
        let cells = raw.iter().cloned().map(formula_safe).collect::<Vec<_>>();
        let row_key = key(&cells);
        ensure!(
            row_key.iter().all(|value| !value.is_empty()),
            "验证记录键不能为空"
        );
        if let Some(index) = complete_group {
            groups.insert(cells[index].clone());
        }
        let bytes = encode_row(&cells)?;
        ensure!(
            expected.insert(row_key.clone(), cells).is_none(),
            "同次保存包含重复验证记录"
        );
        incoming.push((row_key, bytes));
    }
    fs::create_dir_all(directory).context("无法创建验证记录目录")?;
    let path = directory.join(name);
    let lock_path = directory.join(format!("{name}.lock"));
    regular_or_absent(&lock_path)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .context("无法打开验证记录锁")?;
    ensure!(lock.metadata()?.is_file(), "验证记录锁不是普通文件");
    lock.try_lock()
        .context("验证记录正由其他进程保存，或无法取得文件锁，请稍后重试")?;
    regular_or_absent(&path)?;
    let mut existing = match File::open(&path) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("无法读取已有验证记录"),
    };
    let mut seen = BTreeSet::new();
    let mut has_content = false;
    let mut ends_in_newline = false;
    if let Some(file) = existing.as_mut() {
        has_content = file.metadata()?.len() != 0;
        if has_content {
            let mut reader = BufReader::new(&mut *file);
            let mut bom = [0; 3];
            reader
                .read_exact(&mut bom)
                .context("验证记录缺少UTF-8 BOM")?;
            ensure!(bom.as_slice() == BOM, "已有验证记录不是带BOM的UTF-8 CSV");
            let stored_header =
                read_row_with_limit(&mut reader, header.len())?.context("验证记录缺少表头")?;
            ensure!(
                stored_header
                    .iter()
                    .map(String::as_str)
                    .eq(header.iter().copied()),
                "已有验证记录表头不匹配"
            );
            while let Some(row) = read_row_with_limit(&mut reader, header.len())? {
                ensure!(row.len() == header.len(), "已有验证记录列数不匹配");
                ensure!(
                    !row.iter().map(String::as_str).eq(header.iter().copied()),
                    "已有验证记录包含重复表头"
                );
                let row_key = key(&row);
                if let Some(index) = complete_group
                    && groups.contains(&row[index])
                {
                    ensure!(
                        expected.contains_key(&row_key),
                        "已有批次与待保存的设备列表不一致"
                    );
                }
                if let Some(wanted) = expected.get(&row_key) {
                    ensure!(&row == wanted, "同批次记录已有不同验证结果，未覆盖原文件");
                    ensure!(seen.insert(row_key), "已有批次包含重复验证记录");
                }
            }
            drop(reader);
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            ends_in_newline = last[0] == b'\n';
        }
    }
    let missing: Vec<_> = incoming
        .iter()
        .filter(|(key, _)| !seen.contains(key))
        .collect();
    if missing.is_empty() {
        OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.sync_all())
            .context("无法同步已有验证记录")?;
        sync_directory(directory)?;
        return Ok(path);
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).context("无法创建验证记录临时文件")?;
    if has_content {
        let file = existing.as_mut().expect("existing content has a file");
        file.seek(SeekFrom::Start(0))?;
        std::io::copy(file, temporary.as_file_mut()).context("无法复制已有验证记录")?;
        if !ends_in_newline {
            temporary.write_all(b"\r\n")?;
        }
    } else {
        temporary.write_all(BOM)?;
        temporary.write_all(&encode_row(
            &header.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        )?)?;
    }
    for (_, bytes) in missing {
        temporary.write_all(bytes).context("无法写入验证结果")?;
    }
    temporary.flush().context("无法刷新验证记录")?;
    temporary
        .as_file()
        .sync_all()
        .context("无法同步验证记录到磁盘")?;
    drop(existing);
    let committed = temporary
        .persist(&path)
        .map_err(|error| error.error)
        .context("无法原子替换验证记录，原文件保持不变")?;
    committed
        .sync_all()
        .context("验证记录已替换，但同步失败；可重试同一批次")?;
    sync_directory(directory).context("验证记录已替换，但目录同步失败；可重试同一批次")?;
    drop(lock);
    Ok(path)
}

fn regular_or_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "验证记录或锁路径不是普通文件"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("无法检查验证记录路径"),
    }
    Ok(())
}

fn record_cells(record: &VerificationRecord) -> Vec<String> {
    let checks = &record.checks;
    [
        record.batch_id.clone(),
        record.port.clone(),
        record.finished_at.clone(),
        if checks.max30105_ok { "OK" } else { "FAIL" }.into(),
        if checks.bmi270_ok { "OK" } else { "FAIL" }.into(),
        checks.mac.clone().unwrap_or_default(),
        checks.unique_device_id.clone().unwrap_or_default(),
        checks
            .battery_mv
            .map(|value| value.to_string())
            .unwrap_or_default(),
        if checks.passes() { "PASS" } else { "FAIL" }.into(),
        checks.failures.join("\n"),
    ]
    .into_iter()
    .collect()
}

fn formula_safe(value: String) -> String {
    let first = value
        .trim_start_matches(|c: char| c.is_whitespace() || c.is_control() || c == '\u{feff}')
        .chars()
        .next();
    if value.starts_with(['\'', '\t', '\r', '\n'])
        || first.is_some_and(|c| matches!(c, '=' | '+' | '-' | '@'))
    {
        format!("'{value}")
    } else {
        value
    }
}

fn encode_row(cells: &[String]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        if index != 0 {
            bytes.push(b',');
        }
        bytes.push(b'"');
        for byte in cell.bytes() {
            bytes.push(byte);
            if byte == b'"' {
                bytes.push(byte);
            }
            ensure!(bytes.len() <= MAX_ROW_BYTES - 3, "验证记录单行超过2MiB");
        }
        bytes.push(b'"');
    }
    bytes.extend_from_slice(b"\r\n");
    ensure!(bytes.len() <= MAX_ROW_BYTES, "验证记录单行超过2MiB");
    Ok(bytes)
}

// Read complete CSV records, including quoted line breaks, with bounded memory.
#[cfg(test)]
fn read_row(reader: &mut impl BufRead) -> Result<Option<Vec<String>>> {
    read_row_with_limit(reader, MAX_COLUMNS)
}

pub(crate) fn read_row_with_limit(
    reader: &mut impl BufRead,
    max_columns: usize,
) -> Result<Option<Vec<String>>> {
    let mut raw = Vec::new();
    let mut quoted = false;
    loop {
        let start = raw.len();
        let count = (&mut *reader)
            .take((MAX_ROW_BYTES + 1 - start) as u64)
            .read_until(b'\n', &mut raw)
            .context("无法读取验证CSV")?;
        ensure!(raw.len() <= MAX_ROW_BYTES, "已有验证CSV单行超过2MiB");
        for byte in &raw[start..] {
            if *byte == b'"' {
                quoted = !quoted;
            }
        }
        if count == 0 || !quoted {
            break;
        }
    }
    if raw.is_empty() {
        return Ok(None);
    }
    if raw.last() == Some(&b'\n') {
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
    }
    let mut cells = Vec::new();
    let mut field = Vec::new();
    #[derive(Clone, Copy)]
    enum FieldState {
        Start,
        Plain,
        Quoted,
        AfterQuote,
    }
    use FieldState::{AfterQuote, Plain, Quoted, Start};
    let mut state = Start;
    for byte in raw {
        match (state, byte) {
            (Start, b'"') => state = Quoted,
            (Quoted, b'"') => state = AfterQuote,
            (AfterQuote, b'"') => {
                field.push(b'"');
                state = Quoted;
            }
            (Start | Plain | AfterQuote, b',') => {
                cells.push(
                    String::from_utf8(std::mem::take(&mut field))
                        .context("验证CSV包含无效UTF-8")?,
                );
                ensure!(cells.len() < max_columns, "已有验证CSV列数过多");
                state = Start;
            }
            (Quoted, _) => field.push(byte),
            (Start | Plain, b'"' | b'\r' | b'\n') | (AfterQuote, _) => {
                bail!("已有验证 CSV 格式损坏，请检查文件")
            }
            (_, _) => {
                field.push(byte);
                state = Plain;
            }
        }
    }
    ensure!(!matches!(state, Quoted), "已有验证 CSV 引号未闭合");
    cells.push(String::from_utf8(field).context("验证CSV包含无效UTF-8")?);
    Ok(Some(cells))
}

fn sync_directory(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(batch: &str, port: &str, battery_mv: u32) -> VerificationRecord {
        VerificationRecord {
            batch_id: batch.into(),
            port: port.into(),
            finished_at: "2026-01-01T00:00:00Z".into(),
            startup_log: String::new(),
            checks: VerificationChecks {
                max30105_ok: true,
                bmi270_ok: true,
                mac: Some("001122AABBCC".into()),
                unique_device_id: Some("00112233445566778899AABBCCDDEEFF".into()),
                battery_mv: Some(battery_mv),
                failures: Vec::new(),
            },
        }
    }

    fn parsed(bytes: &[u8]) -> Vec<Vec<String>> {
        assert!(bytes.starts_with(BOM));
        let mut reader = BufReader::new(&bytes[BOM.len()..]);
        let mut rows = Vec::new();
        while let Some(row) = read_row(&mut reader).unwrap() {
            rows.push(row);
        }
        rows
    }

    fn production(order: &str) -> ProductionBatch {
        ProductionBatch::parse(order, "2601", "2026-01-01").unwrap()
    }

    #[test]
    fn optional_metadata_keeps_full_logs_without_overwriting_legacy_history() {
        let root = tempfile::tempdir().unwrap();
        let legacy = append_batch(root.path(), &[sample("legacy", "COM1", 2922)]).unwrap();
        let legacy_bytes = fs::read(&legacy).unwrap();
        let mut blank = sample("blank-production", "COM2", 2922);
        blank.startup_log = "ESP-ROM:synthetic\nlast line".into();
        let path =
            append_batch_with_optional_production(root.path(), &[blank.clone()], None).unwrap();
        assert_eq!(path.file_name().unwrap(), "validation-records.csv");
        let before = fs::read(&path).unwrap();
        let rows = parsed(&before);
        assert_eq!(rows[0].len(), 14);
        assert_eq!(&rows[1][10..13], ["", "", ""]);
        assert_eq!(rows[1][13], blank.startup_log);
        let date_only = ProductionBatch::parse(" \t", " ", " 2024-02-29 ").unwrap();
        let later = sample("date-only", "COM3", 2922);
        assert_eq!(
            append_batch_with_optional_production(root.path(), &[later], Some(&date_only)).unwrap(),
            path
        );
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&before));
        assert_eq!(&parsed(&after)[2][10..13], ["", "", "2024-02-29"]);
        assert_eq!(fs::read(legacy).unwrap(), legacy_bytes);
        let order_only = ProductionBatch::parse(" work-order-only ", "", "").unwrap();
        let grouped = append_batch_with_production(
            root.path(),
            &[sample("order-only", "COM4", 2922)],
            &order_only,
        )
        .unwrap();
        assert_ne!(grouped, path);
        assert_eq!(
            &parsed(&fs::read(grouped).unwrap())[1][10..13],
            ["work-order-only", "", ""]
        );
    }

    #[test]
    fn production_orders_have_safe_distinct_files_and_keep_full_frozen_records() {
        let dir = tempfile::tempdir().unwrap();
        let order = production("../synthetic/order:*?工单");
        let mut first = sample("synthetic-prod-a", "COM1", 2922);
        first.finished_at = "2026-01-01T08:00:00+08:00".into();
        first.startup_log = "Synthetic boot\r\nquotes: \"ready\", 逗号\nlast line".into();
        let path =
            append_batch_with_production(dir.path(), std::slice::from_ref(&first), &order).unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(path.parent(), Some(dir.path()));
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!(
                "validation-{:x}.csv",
                Sha256::digest(order.work_order.as_bytes())
            )
        );
        let rows = parsed(&before);
        assert_eq!(&rows[0][..HEADER.len()], HEADER);
        assert_eq!(&rows[0][HEADER.len()..], PRODUCTION_COLUMNS);
        assert_eq!(rows[1][2], first.finished_at);
        assert_eq!(
            &rows[1][10..13],
            [order.work_order.as_str(), "2601", "2026-01-01"]
        );
        assert_eq!(rows[1][13], first.startup_log);
        append_batch_with_production(dir.path(), std::slice::from_ref(&first), &order).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        let second = sample("synthetic-prod-b", "COM2", 2923);
        assert_eq!(
            append_batch_with_production(dir.path(), &[second], &order).unwrap(),
            path
        );
        assert!(fs::read(&path).unwrap().starts_with(&before));
        let other = append_batch_with_production(
            dir.path(),
            &[sample("synthetic-other", "COM3", 2922)],
            &production("synthetic-other-order"),
        )
        .unwrap();
        assert_ne!(other, path);
        assert_eq!(parsed(&fs::read(other).unwrap()).len(), 2);
        let mut changed = first;
        changed.startup_log.push_str(" changed capture");
        let saved = fs::read(&path).unwrap();
        assert!(append_batch_with_production(dir.path(), &[changed], &order).is_err());
        assert_eq!(fs::read(path).unwrap(), saved);
    }

    #[test]
    fn full_512_kib_quoted_startup_log_survives_csv_expansion_and_retry() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = sample("synthetic-large", "COM1", 2922);
        first.startup_log = "\"".repeat(512 * 1024);
        let order = production("synthetic-large-order");
        let path =
            append_batch_with_production(dir.path(), std::slice::from_ref(&first), &order).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(before.len() > 1024 * 1024);
        assert_eq!(parsed(&before)[1][13], first.startup_log);
        append_batch_with_production(dir.path(), &[first], &order).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut oversized = sample("synthetic-oversized", "COM2", 2922);
        oversized.startup_log = "x".repeat(MAX_ROW_BYTES + 1);
        assert!(append_batch_with_production(dir.path(), &[oversized], &order).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn production_format_does_not_upgrade_or_mix_with_legacy_ten_column_files() {
        let dir = tempfile::tempdir().unwrap();
        let row = sample("synthetic-legacy", "COM1", 2922);
        let legacy = append_batch(dir.path(), std::slice::from_ref(&row)).unwrap();
        let before = fs::read(&legacy).unwrap();
        let order = production("synthetic-legacy-order");
        let path =
            append_batch_with_production(dir.path(), std::slice::from_ref(&row), &order).unwrap();
        assert_ne!(legacy, path);
        assert_eq!(fs::read(&legacy).unwrap(), before);
        assert_eq!(parsed(&before)[0].len(), 10);
        assert_eq!(parsed(&fs::read(&path).unwrap())[0].len(), 14);
        fs::write(&path, &before).unwrap();
        assert!(append_batch_with_production(dir.path(), &[row], &order).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
        let unassigned = append_batch_with_production(
            dir.path(),
            &[sample("synthetic-empty", "COM1", 2922)],
            &production("  "),
        )
        .unwrap();
        assert_eq!(unassigned.file_name().unwrap(), "validation-records.csv");
        assert_eq!(
            &parsed(&fs::read(unassigned).unwrap())[1][10..13],
            ["", "2601", "2026-01-01"]
        );
    }

    #[test]
    fn multiple_batches_share_one_header_and_preserve_every_previous_byte() {
        let dir = tempfile::tempdir().unwrap();
        let first = [
            sample("synthetic-a", "COM1", 2922),
            sample("synthetic-a", "COM2", 2923),
        ];
        let path = append_batch(dir.path(), &first).unwrap();
        let before = fs::read(&path).unwrap();
        let second = [
            sample("synthetic-b", "COM1", 2922),
            sample("synthetic-b", "COM2", 2923),
        ];
        assert_eq!(append_batch(dir.path(), &second).unwrap(), path);
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&before));
        let rows = parsed(&after);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0], HEADER);
        assert_eq!(rows[1][8], "PASS");
        assert_eq!(rows[2][7], "2923");
        assert_eq!(rows[2][8], "FAIL");
        assert_eq!(
            after.windows(BOM.len()).filter(|part| *part == BOM).count(),
            1
        );
    }

    #[test]
    fn retries_are_order_independent_and_conflicting_or_duplicate_rows_do_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let first = sample("synthetic-retry", "COM1", 2922);
        let second = sample("synthetic-retry", "COM2", 2922);
        let path = append_batch(dir.path(), std::slice::from_ref(&first)).unwrap();
        append_batch(dir.path(), &[first.clone(), second.clone()]).unwrap();
        let before = fs::read(&path).unwrap();
        append_batch(dir.path(), &[second.clone(), first.clone()]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut changed = first.clone();
        changed.finished_at = "2026-01-01T00:00:01Z".into();
        assert!(append_batch(dir.path(), &[changed, second]).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(append_batch(dir.path(), &[first.clone(), first]).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(parsed(&before).len(), 3);
    }

    #[test]
    fn shared_writer_preserves_complete_device_batch_snapshot_validation() {
        let dir = tempfile::tempdir().unwrap();
        let first = sample("synthetic-complete", "COM1", 2922);
        let second = sample("synthetic-complete", "COM2", 2922);
        let path = append_batch(dir.path(), &[first.clone(), second]).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(append_batch(dir.path(), &[first]).is_err());
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn quoted_multiline_utf8_and_formula_values_remain_safe_on_later_appends() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = sample("=synthetic", "+COM1", 2922);
        first.finished_at = "synthetic,\"time\"\n第二行".into();
        first.checks.failures = vec!["\t=1+1".into(), "引号\"与换行\r\n原因".into()];
        let path = append_batch(dir.path(), std::slice::from_ref(&first)).unwrap();
        let before = fs::read(&path).unwrap();
        append_batch(dir.path(), &[first]).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        append_batch(dir.path(), &[sample("synthetic-next", "COM2", 2922)]).unwrap();
        let rows = parsed(&fs::read(&path).unwrap());
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1][0], "'=synthetic");
        assert_eq!(rows[1][1], "'+COM1");
        assert_eq!(rows[1][2], "synthetic,\"time\"\n第二行");
        assert!(rows[1][9].starts_with("'\t=1+1"));
        assert_eq!(rows[1][8], "FAIL");
        assert_ne!(formula_safe("=x".into()), formula_safe("'=x".into()));
    }

    #[test]
    fn malformed_existing_files_are_never_overwritten() {
        let header =
            encode_row(&HEADER.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()).unwrap();
        for tail in [
            b"\"unterminated\n".as_slice(),
            b"wrong,column,count\r\n",
            b"\xff\r\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(NAME);
            let bytes = [BOM, &header, tail].concat();
            fs::write(&path, &bytes).unwrap();
            assert!(append_batch(dir.path(), &[sample("synthetic-bad", "COM1", 2922)]).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(NAME);
        fs::write(&path, b"not the expected header\r\n").unwrap();
        assert!(append_batch(dir.path(), &[sample("synthetic-bad", "COM1", 2922)]).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not the expected header\r\n");
    }

    #[test]
    fn lock_contention_is_recoverable_and_keeps_prior_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = append_batch(dir.path(), &[sample("synthetic-lock-a", "COM1", 2922)]).unwrap();
        let before = fs::read(&path).unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.path().join("validation-results.csv.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let next = sample("synthetic-lock-b", "COM2", 2922);
        assert!(append_batch(dir.path(), std::slice::from_ref(&next)).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        drop(lock);
        append_batch(dir.path(), &[next]).unwrap();
        assert!(fs::read(&path).unwrap().starts_with(&before));
    }

    #[test]
    fn target_directory_or_invalid_input_never_replaces_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(NAME);
        fs::create_dir(&target).unwrap();
        let sentinel = target.join("preserve.txt");
        fs::write(&sentinel, b"preserve").unwrap();
        assert!(append_batch(dir.path(), &[sample("synthetic-error", "COM1", 2922)]).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
        assert!(append_batch(dir.path(), &[]).is_err());
        assert!(
            append_batch(
                Path::new("relative"),
                &[sample("synthetic-error", "COM1", 2922)]
            )
            .is_err()
        );
    }
}
