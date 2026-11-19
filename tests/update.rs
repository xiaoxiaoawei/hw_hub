use esp32c3_flashtool::updater::{
    CheckResult, UpdateEndpoint, UpdateHttpResponse, UpdateSettings, check_latest_from_json,
    check_latest_with_transport, load_settings_from, save_settings_at, should_auto_check,
};
use serde_json::{Value, json};
use std::{collections::VecDeque, fs};

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

// Public v0.4.2 API fields observed on 2026-11-19; unrelated API fields omitted.
const PUBLISHED_042: &str = r##"{
  "tag_name": "v0.4.2",
  "html_url": "https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2",
  "draft": false,
  "prerelease": false,
  "body": "- Windows 改用原生 USB 与串口接口插拔事件检测，移除固定每两秒自动扫描；空闲时不枚举串口。\n- 插拔后合并事件并执行有限次数的延迟识别，兼容串口驱动稍后就绪；保留手动刷新、批次设备身份保护和逐 COM 进度。\n- 自动检测不可用时明确提示使用手动刷新，不隐藏错误或恢复定时轮询。\n- 修复发布草稿的查询与续传，增加复用成功 Windows 构建产物的发布重试工作流。\n\nWindows x64 构建平台和验证范围见 ZIP 内 BUILD-METADATA.json；USB Hub 多设备烧录及驱动安装仍需实机验收。\n\n驱动包状态：{\"ch343\": \"embedded\", \"cp210x\": \"embedded\"}",
  "assets": [
    {
      "name": "FlashTool-v0.4.2-source.zip",
      "browser_download_url": "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.4.2/FlashTool-v0.4.2-source.zip"
    },
    {
      "name": "FlashTool-v0.4.2-windows-x64.zip",
      "browser_download_url": "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.4.2/FlashTool-v0.4.2-windows-x64.zip"
    },
    {
      "name": "SHA256SUMS",
      "browser_download_url": "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.4.2/SHA256SUMS"
    }
  ]
}"##;

fn response(status: u16, body: impl Into<Vec<u8>>, location: Option<&str>) -> UpdateHttpResponse {
    UpdateHttpResponse {
        status,
        body: body.into(),
        location: location.map(str::to_owned),
    }
}

fn latest_redirect() -> UpdateHttpResponse {
    response(
        302,
        Vec::new(),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2"),
    )
}

// Real expanded_assets/v0.4.2 markup shape, limited to its Windows asset anchor.
const WINDOWS_042_HTML: &str = r#"
<div data-view-component="true" class="Box Box--condensed tmp-mt-3">
  <ul data-view-component="true"><li class="Box-row">
    <a href="/xiaoxiaoawei/hw_hub/releases/download/v0.4.2/FlashTool-v0.4.2-windows-x64.zip" rel="nofollow" data-turbo="false" class="wb-break-all">
      FlashTool-v0.4.2-windows-x64.zip
    </a>
  </li></ul>
</div>
"#;

fn replay(
    current: &str,
    replies: Vec<(UpdateEndpoint, anyhow::Result<UpdateHttpResponse>)>,
) -> anyhow::Result<CheckResult> {
    let mut replies = VecDeque::from(replies);
    let result = check_latest_with_transport(current, |endpoint| {
        let (expected, response) = replies.pop_front().expect("unexpected network request");
        assert_eq!(
            endpoint, expected,
            "request must use only the expected official endpoint"
        );
        response
    });
    assert!(replies.is_empty(), "expected requests were not exercised");
    result
}

fn fallback_replies(
    api: anyhow::Result<UpdateHttpResponse>,
    latest: UpdateHttpResponse,
    assets: UpdateHttpResponse,
) -> Vec<(UpdateEndpoint, anyhow::Result<UpdateHttpResponse>)> {
    vec![
        (UpdateEndpoint::ApiLatest, api),
        (UpdateEndpoint::WebsiteLatest, Ok(latest)),
        (
            UpdateEndpoint::ExpandedAssets {
                tag: "v0.4.2".into(),
            },
            Ok(assets),
        ),
    ]
}

#[test]
fn real_published_042_sample_is_up_to_date_without_any_fallback_or_downgrade() {
    for current in ["0.4.2", "0.4.3"] {
        assert_eq!(
            replay(
                current,
                vec![(
                    UpdateEndpoint::ApiLatest,
                    Ok(response(200, PUBLISHED_042, None))
                )]
            )
            .unwrap(),
            CheckResult::UpToDate {
                latest_version: "0.4.2".into()
            }
        );
    }
}

#[test]
fn transient_api_failures_use_only_verified_official_latest_and_asset_proof() {
    for status in [403, 429, 500, 502, 503, 504] {
        assert_eq!(
            replay(
                "0.4.2",
                fallback_replies(
                    Ok(response(status, Vec::new(), None)),
                    latest_redirect(),
                    response(200, WINDOWS_042_HTML, None),
                )
            )
            .unwrap(),
            CheckResult::UpToDate {
                latest_version: "0.4.2".into()
            }
        );
    }
    let CheckResult::Available(update) = replay(
        "0.4.1",
        fallback_replies(
            Err(anyhow::anyhow!("WinHTTP 12029: cannot connect")),
            latest_redirect(),
            response(200, WINDOWS_042_HTML, None),
        ),
    )
    .unwrap() else {
        panic!("verified newer release should be available");
    };
    assert_eq!(update.version, "0.4.2");
    assert_eq!(
        update.release_url,
        "https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2"
    );
    assert_eq!(
        update.download_url.as_deref(),
        Some(
            "https://github.com/xiaoxiaoawei/hw_hub/releases/download/v0.4.2/FlashTool-v0.4.2-windows-x64.zip"
        )
    );
    assert!(update.notes.is_empty());
}

#[test]
fn an_api_recovery_on_the_next_manual_check_needs_no_preference_or_state_change() {
    let mut replies = VecDeque::from(fallback_replies(
        Err(anyhow::anyhow!("temporary DNS failure")),
        latest_redirect(),
        response(200, WINDOWS_042_HTML, None),
    ));
    replies.push_back((
        UpdateEndpoint::ApiLatest,
        Ok(response(200, PUBLISHED_042, None)),
    ));
    let mut transport = |endpoint| {
        let (expected, response) = replies.pop_front().expect("unexpected request");
        assert_eq!(endpoint, expected);
        response
    };
    for _ in 0..2 {
        assert_eq!(
            check_latest_with_transport("0.4.2", &mut transport).unwrap(),
            CheckResult::UpToDate {
                latest_version: "0.4.2".into()
            }
        );
    }
    assert!(replies.is_empty());
}

#[test]
fn api_not_found_requires_website_confirmation_and_network_errors_stay_errors() {
    assert_eq!(
        replay(
            "0.4.2",
            vec![
                (
                    UpdateEndpoint::ApiLatest,
                    Ok(response(404, Vec::new(), None))
                ),
                (
                    UpdateEndpoint::WebsiteLatest,
                    Ok(response(404, Vec::new(), None))
                ),
            ]
        )
        .unwrap(),
        CheckResult::NoPublishedRelease
    );
    for api in [
        Ok(response(404, Vec::new(), None)),
        Err(anyhow::anyhow!("API DNS failure")),
    ] {
        let error = replay(
            "0.4.2",
            vec![
                (UpdateEndpoint::ApiLatest, api),
                (
                    UpdateEndpoint::WebsiteLatest,
                    Err(anyhow::anyhow!("website WinHTTP 12002 timeout")),
                ),
            ],
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("GitHub API")
                && message.contains("GitHub 官网")
                && message.contains("12002")
        );
        assert!(message.contains("HTTP 404") || message.contains("API DNS failure"));
    }
    assert!(
        replay(
            "0.4.2",
            vec![
                (
                    UpdateEndpoint::ApiLatest,
                    Err(anyhow::anyhow!("API connection failure"))
                ),
                (
                    UpdateEndpoint::WebsiteLatest,
                    Ok(response(404, Vec::new(), None))
                ),
            ]
        )
        .is_err()
    );
}

#[test]
fn invalid_or_unsafe_successful_api_data_never_triggers_website_fallback() {
    let mut inputs = vec!["not JSON".to_owned(), "{}".to_owned()];
    for flag in ["draft", "prerelease"] {
        let mut input = release("0.4.2", true);
        input[flag] = true.into();
        inputs.push(input.to_string());
    }
    let mut input = release("0.4.2", true);
    input["html_url"] = "https://github.com/someone/other/releases/tag/v0.4.2".into();
    inputs.push(input.to_string());
    for input in inputs {
        assert!(
            replay(
                "0.4.2",
                vec![(UpdateEndpoint::ApiLatest, Ok(response(200, input, None)))]
            )
            .is_err()
        );
    }
    assert!(
        replay(
            "0.4.2",
            vec![(
                UpdateEndpoint::ApiLatest,
                Ok(response(302, Vec::new(), Some("https://evil.example/")))
            )]
        )
        .is_err()
    );
}

#[test]
fn fallback_rejects_unsafe_nonstable_or_missing_latest_redirects_before_fetching_assets() {
    for location in [
        None,
        Some("http://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2"),
        Some("https://github.com.evil.example/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2"),
        Some("https://github.com@evil.example/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2"),
        Some("//evil.example/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2"),
        Some("https://github.com/someone/other/releases/tag/v0.4.2"),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/edit/v0.4.2"),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2-rc.1"),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2+build"),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v00.4.2"),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2?redirect=evil"),
        Some("https://github.com/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2/../v0.5.0"),
    ] {
        assert!(
            replay(
                "0.4.2",
                vec![
                    (
                        UpdateEndpoint::ApiLatest,
                        Ok(response(429, Vec::new(), None))
                    ),
                    (
                        UpdateEndpoint::WebsiteLatest,
                        Ok(response(302, Vec::new(), location))
                    ),
                ]
            )
            .is_err(),
            "must reject {location:?}"
        );
    }
    for status in [200, 403, 404, 429, 500] {
        // HTML labelled Draft is never accepted as a latest-release proof.
        assert!(
            replay(
                "0.4.2",
                vec![
                    (
                        UpdateEndpoint::ApiLatest,
                        Ok(response(503, Vec::new(), None))
                    ),
                    (
                        UpdateEndpoint::WebsiteLatest,
                        Ok(response(status, "<span>Draft</span>", None))
                    ),
                ]
            )
            .is_err()
        );
    }
}

#[test]
fn fallback_asset_proof_is_canonical_unique_and_never_redirected_or_extracted_from_text() {
    let relative = "/xiaoxiaoawei/hw_hub/releases/download/v0.4.2/FlashTool-v0.4.2-windows-x64.zip";
    for html in [
        "<div>Draft</div>".to_owned(),
        format!("<!-- <a href=\"{relative}\">fake</a> -->"),
        format!("<script>const s = '<a href=\"{relative}\">fake</a>';</script>"),
        format!("<template><a href=\"{relative}\">fake</a></template>"),
        format!("<a data-href=\"{relative}\">fake</a>"),
        format!("<a href={relative}>unquoted</a>"),
        format!("<a href=\"https://evil.example{relative}\">wrong host</a>"),
        format!("<a href=\"{relative}?redirect=evil\">query</a>"),
        WINDOWS_042_HTML.replace("xiaoxiaoawei/hw_hub", "someone/other"),
        WINDOWS_042_HTML.repeat(2),
        format!("<a href=\"{relative}\" href=\"https://evil.example\">ambiguous</a>"),
        format!("<a href=\"{relative}"),
    ] {
        assert!(
            replay(
                "0.4.1",
                fallback_replies(
                    Ok(response(403, Vec::new(), None)),
                    latest_redirect(),
                    response(200, html.as_bytes(), None),
                )
            )
            .is_err(),
            "must reject {html}"
        );
    }
    for status in [302, 404, 503] {
        assert!(
            replay(
                "0.4.1",
                fallback_replies(
                    Ok(response(403, Vec::new(), None)),
                    latest_redirect(),
                    response(status, WINDOWS_042_HTML, Some("https://evil.example/")),
                )
            )
            .is_err()
        );
    }
    let reordered = format!("<a rel='nofollow' HREF = '{relative}' class='asset'>Windows</a>");
    assert!(matches!(
        replay(
            "0.4.1",
            fallback_replies(
                Ok(response(429, Vec::new(), None)),
                response(
                    302,
                    Vec::new(),
                    Some("/xiaoxiaoawei/hw_hub/releases/tag/v0.4.2")
                ),
                response(200, reordered, None),
            )
        )
        .unwrap(),
        CheckResult::Available(_)
    ));
}

#[test]
fn fallback_responses_remain_bounded_and_require_utf8() {
    for assets in [
        response(200, vec![b'x'; 1024 * 1024 + 1], None),
        response(200, vec![0xff], None),
    ] {
        assert!(
            replay(
                "0.4.2",
                fallback_replies(
                    Err(anyhow::anyhow!("API network failure")),
                    latest_redirect(),
                    assets,
                )
            )
            .is_err()
        );
    }
    let oversized_location = "x".repeat(8193);
    assert!(
        replay(
            "0.4.2",
            vec![
                (
                    UpdateEndpoint::ApiLatest,
                    Ok(response(503, Vec::new(), None))
                ),
                (
                    UpdateEndpoint::WebsiteLatest,
                    Ok(response(302, Vec::new(), Some(&oversized_location)))
                ),
            ]
        )
        .is_err()
    );
}
