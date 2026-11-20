//! Pure checks over one device's standalone firmware output lines.
//! No serial commands, hardware writes, filesystem or network access occur here.
use std::collections::BTreeSet;

pub const MAX_BATTERY_MV: u32 = 2922;

const MAX30105_OK: &str = "MAX30105 detected & working!";
const BMI270_OK: &str = "BMI270 detected & working!";
const MAC_PREFIX: &str = "MAC ADDRESS:";
const UNIQUE_ID_PREFIX: &str = "UNIQUE DEVICE ID:";
const BATTERY_PREFIX: &str = "Analog Read in millivolts at battery pin:";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VerificationChecks {
    pub max30105_ok: bool,
    pub bmi270_ok: bool,
    /// Twelve uppercase hex digits; colon and compact input normalize alike.
    pub mac: Option<String>,
    /// Exact 128-bit output, preserving leading zeros. No MAC substitution.
    pub unique_device_id: Option<String>,
    /// A unique parsed reading is retained even when above MAX_BATTERY_MV.
    pub battery_mv: Option<u32>,
    pub failures: Vec<String>,
}

impl VerificationChecks {
    pub fn passes(&self) -> bool {
        self.max30105_ok
            && self.bmi270_ok
            && self.mac.is_some()
            && self.unique_device_id.is_some()
            && self.battery_mv.is_some_and(|value| value <= MAX_BATTERY_MV)
            && self.failures.is_empty()
    }
}

/// Evaluate only recognized, trimmed standalone lines from this one device.
/// Identical repetitions are harmless; malformed recognized values and
/// conflicting values cannot be repaired by a later good line.
pub fn evaluate(lines: &[String]) -> VerificationChecks {
    let mut checks = VerificationChecks::default();
    let (mut max30105_failed, mut bmi270_failed) = (false, false);
    let (mut mac_seen, mut id_seen, mut battery_seen) = (false, false, false);
    let (mut mac_invalid, mut id_invalid, mut battery_invalid) = (false, false, false);
    let mut macs = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut batteries = BTreeSet::new();

    for line in lines {
        let line = line.trim();
        if line == MAX30105_OK {
            checks.max30105_ok = true;
        } else if line.starts_with("MAX30105 was not found.") {
            max30105_failed = true;
        }
        if line == BMI270_OK {
            checks.bmi270_ok = true;
        } else if line.starts_with("Error: BMI270 not connected, check wiring and I2C address!") {
            bmi270_failed = true;
        }
        if let Some(value) = line.strip_prefix(MAC_PREFIX) {
            mac_seen = true;
            match normalize_mac(value.trim()) {
                Some(value) => {
                    macs.insert(value);
                }
                None => mac_invalid = true,
            }
        }
        if let Some(value) = line.strip_prefix(UNIQUE_ID_PREFIX) {
            id_seen = true;
            let value = value.trim();
            if value.len() == 32
                && value.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !value.bytes().all(|byte| byte == b'0')
            {
                ids.insert(value.to_ascii_uppercase());
            } else {
                id_invalid = true;
            }
        }
        if let Some(value) = line.strip_prefix(BATTERY_PREFIX) {
            battery_seen = true;
            let value = value.trim();
            if !value.is_empty()
                && value.bytes().all(|byte| byte.is_ascii_digit())
                && let Ok(value) = value.parse::<u32>()
            {
                batteries.insert(value);
            } else {
                battery_invalid = true;
            }
        }
    }

    if max30105_failed {
        checks.max30105_ok = false;
        checks.failures.push("MAX30105 报告未找到设备".into());
    } else if !checks.max30105_ok {
        checks.failures.push("缺少 MAX30105 成功检测输出".into());
    }
    if bmi270_failed {
        checks.bmi270_ok = false;
        checks.failures.push("BMI270 报告连接失败".into());
    } else if !checks.bmi270_ok {
        checks.failures.push("缺少 BMI270 成功检测输出".into());
    }

    checks.mac = resolve_value(
        macs,
        mac_seen,
        mac_invalid,
        "MAC ADDRESS",
        &mut checks.failures,
    );
    checks.unique_device_id = resolve_value(
        ids,
        id_seen,
        id_invalid,
        "UNIQUE DEVICE ID",
        &mut checks.failures,
    );
    checks.battery_mv = resolve_value(
        batteries,
        battery_seen,
        battery_invalid,
        "电池毫伏值",
        &mut checks.failures,
    );
    if checks
        .battery_mv
        .is_some_and(|value| value > MAX_BATTERY_MV)
    {
        checks
            .failures
            .push(format!("电池毫伏值不在 0–{MAX_BATTERY_MV} 范围内"));
    }
    checks
}

fn resolve_value<T: Ord>(
    values: BTreeSet<T>,
    seen: bool,
    invalid: bool,
    label: &str,
    failures: &mut Vec<String>,
) -> Option<T> {
    if !seen {
        failures.push(format!("缺少 {label} 输出"));
    } else if invalid {
        failures.push(format!("{label} 输出格式无效或为空白值"));
    } else if values.len() != 1 {
        failures.push(format!("{label} 输出包含多个不同值"));
    } else {
        return values.into_iter().next();
    }
    None
}

fn normalize_mac(value: &str) -> Option<String> {
    let compact = if value.contains(':') {
        let parts: Vec<_> = value.split(':').collect();
        if parts.len() != 6 || parts.iter().any(|part| part.len() != 2) {
            return None;
        }
        parts.concat()
    } else {
        value.to_owned()
    };
    if compact.len() != 12
        || !compact.bytes().all(|byte| byte.is_ascii_hexdigit())
        || compact.bytes().all(|byte| byte == b'0')
        || compact.bytes().all(|byte| byte.eq_ignore_ascii_case(&b'f'))
    {
        return None;
    }
    Some(compact.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic values only. These do not identify a customer's device.
    const MAC: &str = "001122AABBCC";
    const ID: &str = "00112233445566778899AABBCCDDEEFF";

    fn complete(battery: &str) -> Vec<String> {
        [
            MAX30105_OK.into(),
            BMI270_OK.into(),
            format!("{MAC_PREFIX} {MAC}"),
            format!("{UNIQUE_ID_PREFIX} {ID}"),
            format!("{BATTERY_PREFIX} {battery}"),
        ]
        .into()
    }

    #[test]
    fn complete_standalone_output_passes_and_preserves_leading_zeros() {
        let mut lines = complete("2922");
        for line in &mut lines {
            *line = format!("  {line}\t");
        }
        let result = evaluate(&lines);
        assert!(result.passes(), "{:?}", result.failures);
        assert_eq!(result.mac.as_deref(), Some(MAC));
        assert_eq!(result.unique_device_id.as_deref(), Some(ID));
        assert_eq!(result.battery_mv, Some(MAX_BATTERY_MV));
        assert!(evaluate(&complete("0")).passes());
        assert!(!VerificationChecks::default().passes());
    }

    #[test]
    fn identical_repeats_and_colon_mac_normalize_to_one_value() {
        let mut lines = complete("2000");
        lines.extend(complete("02000"));
        lines.push("MAC ADDRESS: 00:11:22:aa:bb:cc".into());
        lines.push(format!("{UNIQUE_ID_PREFIX} {}", ID.to_ascii_lowercase()));
        assert!(evaluate(&lines).passes());
    }

    #[test]
    fn battery_over_range_is_retained_without_using_temperature_or_other_analog_values() {
        let result = evaluate(&complete("2923"));
        assert!(!result.passes());
        assert_eq!(result.battery_mv, Some(2923));
        assert_eq!(result.failures.len(), 1);
        let mut lines = complete("2000");
        lines.pop();
        lines.extend([
            "Temperature: 2000".into(),
            "Analog Read in millivolts at temperature pin: 2000".into(),
        ]);
        let result = evaluate(&lines);
        assert_eq!(result.battery_mv, None);
        assert!(
            result
                .failures
                .iter()
                .any(|error| error.contains("缺少 电池"))
        );
    }

    #[test]
    fn malformed_battery_is_different_from_a_conflicting_or_missing_reading() {
        for value in ["", "-1", "+1", "2000 mV", "2.5", "4294967296"] {
            let result = evaluate(&complete(value));
            assert!(!result.passes());
            assert_eq!(result.battery_mv, None);
            assert!(
                result
                    .failures
                    .iter()
                    .any(|error| error.contains("格式无效")),
                "{value}"
            );
        }
        let mut lines = complete("2000");
        lines.push(format!("{BATTERY_PREFIX} 2001"));
        let result = evaluate(&lines);
        assert_eq!(result.battery_mv, None);
        assert!(
            result
                .failures
                .iter()
                .any(|error| error.contains("多个不同值"))
        );
    }

    #[test]
    fn conflicting_ids_or_a_malformed_identity_cannot_be_repaired_by_a_good_line() {
        for line in [
            "MAC ADDRESS: 001122AABBCD",
            "UNIQUE DEVICE ID: FFEEDDCCBBAA99887766554433221100",
        ] {
            let mut lines = complete("2000");
            lines.push(line.into());
            let result = evaluate(&lines);
            assert!(!result.passes());
            assert!(
                result
                    .failures
                    .iter()
                    .any(|error| error.contains("多个不同值"))
            );
        }
        for line in [
            "MAC ADDRESS: ",
            "MAC ADDRESS: 000000000000",
            "MAC ADDRESS: ff:ff:ff:ff:ff:ff",
            "MAC ADDRESS: 00:11:22:AA:BB:C",
            "UNIQUE DEVICE ID: 00000000000000000000000000000000",
            "UNIQUE DEVICE ID: 00112233445566778899AABBCCDDEEFF trailing",
        ] {
            let mut lines = complete("2000");
            lines.insert(0, line.into());
            let result = evaluate(&lines);
            assert!(!result.passes(), "{line}");
            assert!(
                result
                    .failures
                    .iter()
                    .any(|error| error.contains("格式无效"))
            );
        }
        let mut lines = complete("2000");
        lines[3] = format!("{UNIQUE_ID_PREFIX} {}", "F".repeat(32));
        assert!(evaluate(&lines).passes(), "nonzero all-FF ID is preserved");
    }

    #[test]
    fn success_must_be_exact_and_known_sensor_failure_wins_in_either_order() {
        for (index, failure) in [
            (0, "MAX30105 was not found. Please check wiring/power."),
            (
                1,
                "Error: BMI270 not connected, check wiring and I2C address!",
            ),
        ] {
            for first in [true, false] {
                let mut lines = complete("2000");
                if first {
                    lines.insert(0, failure.into());
                } else {
                    lines.push(failure.into());
                }
                let result = evaluate(&lines);
                assert!(!result.passes());
                assert!(
                    !(if index == 0 {
                        result.max30105_ok
                    } else {
                        result.bmi270_ok
                    })
                );
            }
        }
        for suffix in [" extra", "\nother output"] {
            let mut lines = complete("2000");
            lines[0].push_str(suffix);
            assert!(!evaluate(&lines).max30105_ok);
        }
    }

    #[test]
    fn metadata_or_inline_labels_do_not_substitute_for_standalone_output() {
        let lines: Vec<_> = complete("2000")
            .into_iter()
            .map(|line| format!("[INFO] {line}"))
            .collect();
        let result = evaluate(&lines);
        assert!(!result.passes());
        assert_eq!(result.failures.len(), 5);
        assert!(
            result.mac.is_none()
                && result.unique_device_id.is_none()
                && result.battery_mv.is_none()
        );
    }
}
