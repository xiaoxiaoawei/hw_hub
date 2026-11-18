use esp32c3_flashtool::updater::{
    CheckResult, UpdateSettings, check_latest_from_json, load_settings_from, save_settings_at,
    should_auto_check,
};
use serde_json::{Value, json};
use std::fs;

fn release(version: &str, with_asset: bool) -> Value {
    let name = format!("FlashTool-v{version}-windows-x64.zip");
    json!({
        "tag_name": format!("v{version}"),
        "html_url": format!("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v{version}"),
        "draft": false,
        "prerelease": false,
        "body": "新增芯片支持与批量烧录改进。",
        "assets": if with_asset {
            vec![json!({"name": name, "browser_download_url": format!("https://github.com/xiaoxiaoawei/hw_hub/releases/download/v{version}/{name}")})]
        } else { vec![] },
    })
}

fn check(current: &str, release: &Value) -> anyhow::Result<CheckResult> {
    check_latest_from_json(current, &release.to_string())
}

#[test]
fn compares_semver_numerically_and_never_recommends_a_downgrade() {
    let CheckResult::Available(update) = check("0.9.9", &release("0.10.0", true)).unwrap() else {
        panic!("0.10 must be newer than 0.9")
    };
    assert_eq!(update.version, "0.10.0");
    assert_eq!(
        update.download_url.as_deref(),
        Some(
            "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.10.0/FlashTool-v0.10.0-windows-x64.zip"
        )
    );
    for installed in ["0.10.0", "0.11.0", "1.0.0"] {
        assert_eq!(
            check(installed, &release("0.10.0", true)).unwrap(),
            CheckResult::UpToDate {
                latest_version: "0.10.0".into()
            }
        );
    }
    assert!(matches!(
        check("0.3.0-beta.1", &release("0.3.0", true)).unwrap(),
        CheckResult::Available(_)
    ));
}

#[test]
fn missing_windows_asset_reports_the_release_without_inventing_a_download() {
    let mut value = release("0.4.0", false);
    value["assets"] = json!([{"name":"FlashTool-v0.4.0-linux.zip", "browser_download_url":"https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.4.0/FlashTool-v0.4.0-linux.zip"}]);
    let CheckResult::Available(update) = check("0.3.0", &value).unwrap() else {
        panic!("expected release")
    };
    assert!(update.download_url.is_none());
    assert_eq!(
        update.release_url,
        "https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.0"
    );
    value["assets"] = json!([]);
    value["body"] = Value::Null;
    let CheckResult::Available(update) = check("0.3.0", &value).unwrap() else {
        panic!("expected release")
    };
    assert!(update.download_url.is_none());
    assert!(update.notes.is_empty());
}

#[test]
fn only_formal_stable_canonical_v_semver_tags_are_accepted() {
    for tag in [
        "0.4.0",
        "v0.4",
        "v00.4.0",
        "v0.4.0-rc.1",
        "v0.4.0+build",
        "v0.4.0 ",
        "vnext",
    ] {
        let mut value = release("0.4.0", true);
        value["tag_name"] = tag.into();
        assert!(check("0.3.0", &value).is_err(), "must reject {tag}");
    }
    for flag in ["draft", "prerelease"] {
        let mut value = release("0.4.0", true);
        value[flag] = true.into();
        assert!(check("0.3.0", &value).is_err());
    }
    assert!(check("not-semver", &release("0.4.0", true)).is_err());
}

#[test]
fn refuses_release_links_outside_the_exact_repository_and_tag() {
    for url in [
        "http://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.0",
        "https://github.com.evil.example/xiaoxiaoawei/hw_hub/releases/tag/v0.4.0",
        "https://github.com@evil.example/xiaoxiaoawei/hw_hub/releases/tag/v0.4.0",
        "https://github.com/someone/other/releases/tag/v0.4.0",
        "https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.5.0",
        "https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.0?redirect=evil",
    ] {
        let mut value = release("0.4.0", true);
        value["html_url"] = url.into();
        assert!(check("0.3.0", &value).is_err(), "must reject {url}");
    }
}

#[test]
fn refuses_download_link_substitution_and_ambiguous_assets() {
    for url in [
        "https://evil.example/FlashTool-v0.4.0-windows-x64.zip",
        "https://github.com/someone/other/releases/download/v0.4.0/FlashTool-v0.4.0-windows-x64.zip",
        "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.5.0/FlashTool-v0.4.0-windows-x64.zip",
        "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.4.0/FlashTool-v0.4.0-windows-x64.zip?redirect=evil",
    ] {
        let mut value = release("0.4.0", true);
        value["assets"][0]["browser_download_url"] = url.into();
        assert!(check("0.3.0", &value).is_err());
    }
    let mut value = release("0.4.0", true);
    let duplicate = value["assets"][0].clone();
    value["assets"].as_array_mut().unwrap().push(duplicate);
    assert!(check("0.3.0", &value).is_err());
}

#[test]
fn malformed_missing_fields_and_oversized_json_are_rejected() {
    assert!(check_latest_from_json("0.3.0", "not json").is_err());
    assert!(check_latest_from_json("0.3.0", "{}").is_err());
    let mut value = release("0.4.0", true);
    value.as_object_mut().unwrap().remove("draft");
    assert!(check("0.3.0", &value).is_err());
    let mut value = release("0.4.0", true);
    value["body"] = "x".repeat(1024 * 1024).into();
    assert!(check("0.3.0", &value).is_err());
}

#[test]
fn automatic_checks_use_both_attempt_and_success_times_with_a_daily_limit() {
    let mut settings = UpdateSettings::default();
    assert!(should_auto_check(&settings, 0));
    settings.last_auto_attempt_unix = Some(1000);
    assert!(settings.last_checked_unix.is_none());
    assert!(!should_auto_check(&settings, 1000 + 86_399));
    assert!(should_auto_check(&settings, 1000 + 86_400));
    settings.last_checked_unix = Some(2000);
    assert!(!should_auto_check(&settings, 1000 + 86_400));
    assert!(should_auto_check(&settings, 2000 + 86_400));
    assert!(!should_auto_check(&settings, 10)); // Clock moving backwards cannot cause a retry loop.
    settings.auto_check = false;
    assert!(!should_auto_check(&settings, 2000 + 2 * 86_400));
}

#[test]
fn settings_round_trip_replace_existing_file_and_migrate_old_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("FlashTool").join("update.json");
    assert_eq!(
        load_settings_from(&path).unwrap(),
        UpdateSettings::default()
    );
    let mut settings = UpdateSettings {
        auto_check: false,
        last_checked_unix: Some(123),
        last_auto_attempt_unix: Some(456),
    };
    save_settings_at(&path, &settings).unwrap();
    assert_eq!(load_settings_from(&path).unwrap(), settings);
    settings.auto_check = true;
    settings.last_checked_unix = Some(789);
    save_settings_at(&path, &settings).unwrap();
    assert_eq!(load_settings_from(&path).unwrap(), settings);
    fs::write(&path, r#"{"auto_check":false,"last_checked_unix":42}"#).unwrap();
    let old = load_settings_from(&path).unwrap();
    assert!(!old.auto_check);
    assert_eq!(old.last_checked_unix, Some(42));
    assert!(old.last_auto_attempt_unix.is_none());
    fs::write(&path, "{}").unwrap();
    assert!(load_settings_from(&path).unwrap().auto_check);
}

#[test]
fn corrupt_settings_and_write_failures_are_visible_without_erasing_old_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("update.json");
    fs::write(&path, b"broken preferences").unwrap();
    assert!(load_settings_from(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"broken preferences");
    fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
    assert!(load_settings_from(&path).is_err());
    let occupied = dir.path().join("directory");
    fs::create_dir(&occupied).unwrap();
    assert!(save_settings_at(&occupied, &UpdateSettings::default()).is_err());
    assert!(occupied.is_dir());
}
