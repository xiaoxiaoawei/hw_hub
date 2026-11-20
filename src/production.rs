//! Validated production metadata and the fixed Beijing display timezone.
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, NaiveDate, Utc};

use crate::telemetry::RecordOperation;

pub const PRODUCTION_HEADERS: [&str; 3] = ["生产批次/工单号", "主板年周", "主板生产日期"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductionBatch {
    pub work_order: String,
    pub board_week: String,
    pub board_date: NaiveDate,
}

impl ProductionBatch {
    pub fn parse(work_order: &str, board_week: &str, board_date: &str) -> Result<Self> {
        let work_order = work_order.trim();
        let board_week = board_week.trim();
        let board_date = board_date.trim();
        ensure!(!work_order.is_empty(), "请填写生产批次/工单号");
        ensure!(
            board_week.len() == 4 && board_week.bytes().all(|byte| byte.is_ascii_digit()),
            "主板年周必须为四位数字 YYWW"
        );
        let week = board_week.as_bytes();
        let week_number = (week[2] - b'0') * 10 + week[3] - b'0';
        ensure!(
            (1..=53).contains(&week_number),
            "主板年周的周数必须为 01–53"
        );
        ensure!(
            board_date.len() == 10
                && board_date.bytes().enumerate().all(|(index, byte)| {
                    if matches!(index, 4 | 7) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_digit()
                    }
                }),
            "主板生产日期必须为 YYYY-MM-DD"
        );
        let board_date = NaiveDate::parse_from_str(board_date, "%Y-%m-%d")
            .context("主板生产日期不是有效日期")?;
        Ok(Self {
            work_order: work_order.to_owned(),
            board_week: board_week.to_owned(),
            board_date,
        })
    }

    /// Join validated metadata to the configured root without filesystem I/O.
    /// Construct this value through `parse` to validate the week component.
    pub fn record_directory(&self, root: &Path, operation: RecordOperation) -> PathBuf {
        root.join(&self.board_week).join(match operation {
            RecordOperation::Flash => "烧录",
            RecordOperation::ReadOnly => "验证",
        })
    }
}

pub fn beijing_datetime(utc: DateTime<Utc>) -> DateTime<FixedOffset> {
    utc.with_timezone(&FixedOffset::east_opt(8 * 60 * 60).expect("UTC+08 is a valid fixed offset"))
}

pub fn beijing_now() -> DateTime<FixedOffset> {
    beijing_datetime(Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn trims_production_inputs_and_accepts_real_leap_dates_without_week_coupling() {
        let batch = ProductionBatch::parse("  工单-001  ", " 2653 ", " 2024-02-29 ").unwrap();
        assert_eq!(batch.work_order, "工单-001");
        assert_eq!(batch.board_week, "2653");
        assert_eq!(
            batch.board_date,
            NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()
        );
        assert!(ProductionBatch::parse("订单", "0001", "2026-11-20").is_ok());
        assert!(ProductionBatch::parse(" \t\n", "2601", "2026-11-20").is_err());
    }

    #[test]
    fn rejects_invalid_week_numbers_non_ascii_and_path_components() {
        for week in [
            "2600",
            "2654",
            "2699",
            "261",
            "26001",
            "26/1",
            "26\\1",
            "../1",
            "２６０１",
            "26-1",
            "26 1",
            "26\x001",
        ] {
            assert!(
                ProductionBatch::parse("order", week, "2026-11-20").is_err(),
                "accepted invalid week: {week:?}"
            );
        }
    }

    #[test]
    fn rejects_impossible_dates_and_non_canonical_date_shapes() {
        for date in [
            "2023-02-29",
            "2024-02-30",
            "2026-13-01",
            "2026-00-01",
            "2026-11-00",
            "2026-11-31",
            "2026-1-01",
            "2026-01-1",
            "2026/01/01",
            "26-01-01",
            "２０２６-01-01",
            "2026-01-01T00:00:00",
            "",
        ] {
            assert!(
                ProductionBatch::parse("order", "2601", date).is_err(),
                "accepted invalid date: {date:?}"
            );
        }
    }

    #[test]
    fn operation_directories_use_only_the_validated_week_and_operation() {
        let batch =
            ProductionBatch::parse("order/with\\punctuation", "2601", "2026-11-20").unwrap();
        let root = Path::new("configured-records");
        assert_eq!(
            batch.record_directory(root, RecordOperation::Flash),
            root.join("2601").join("烧录")
        );
        assert_eq!(
            batch.record_directory(root, RecordOperation::ReadOnly),
            root.join("2601").join("验证")
        );
    }

    #[test]
    fn beijing_conversion_rolls_over_calendar_year_without_changing_the_instant() {
        let utc = Utc.with_ymd_and_hms(2026, 12, 31, 16, 5, 7).unwrap();
        let beijing = beijing_datetime(utc);
        assert_eq!(
            beijing.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2027-01-01 00:05:07"
        );
        assert_eq!(beijing.offset().local_minus_utc(), 8 * 60 * 60);
        assert_eq!(beijing.with_timezone(&Utc), utc);
    }
}
