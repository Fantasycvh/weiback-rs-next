//! Sidecar supervisor 集成测试。
//!
//! 依赖真实 Python sidecar（`sidecar/weiback_collector`）。测试通过环境变量
//! `WEIBACK_COLLECTOR_PYTHON` 或常见候选路径定位 Python；找不到时跳过
//! 该轮测试，避免在无 Python 的 CI 环境误报失败。
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use weiback::sidecar::{
    CommandEnvelope, CommandType, EventType, Sidecar, SidecarError, SpawnOptions,
    protocol::{self, new_uuid_v7},
};

/// 仓库根目录（`tests/` 的上两级）。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// 定位可用的 Python 解释器。
fn python() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = std::env::var("WEIBACK_COLLECTOR_PYTHON")
        .map(PathBuf::from)
        .into_iter()
        .chain([
            PathBuf::from("F:\\build\\projects\\weiback-python\\.venv\\Scripts\\python.exe"),
            PathBuf::from("python"),
            PathBuf::from("python3"),
        ])
        .collect();
    candidates.into_iter().find(|p| {
        Command::new(p)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// 启动真实 Python sidecar 并完成握手的选项。
fn collector_options(python: &Path) -> SpawnOptions {
    let root = repo_root();
    SpawnOptions {
        program: python.to_path_buf(),
        args: vec!["-u".into(), "-m".into(), "weiback_collector".into()],
        env: vec![
            (
                "PYTHONPATH".into(),
                root.join("sidecar").to_string_lossy().to_string(),
            ),
            ("PYTHONUTF8".into(), "1".into()),
            ("WEIBACK_COLLECTOR_MODE".into(), "fixture".into()),
        ],
        cwd: Some(root),
        handshake_timeout: Duration::from_secs(10),
    }
}

/// 握手成功：ready + capabilities 均收到。
#[test]
fn handshake_succeeds_with_real_collector() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let (mut sidecar, ready, capabilities) =
        Sidecar::spawn_with_handshake(&collector_options(&py)).expect("handshake should succeed");

    assert_eq!(ready["sidecar_name"], "weiback-collector");
    assert_eq!(ready["protocol_version"], 1);
    assert!(ready["sidecar_version"].is_string());

    let versions = capabilities["protocol_versions"].as_array().expect("protocol_versions");
    assert!(versions.contains(&serde_json::json!(1)));

    let commands = capabilities["commands"].as_array().expect("commands");
    assert!(commands.iter().any(|c| c == "hello"));

    sidecar.shutdown(Duration::from_millis(500)).expect("clean shutdown");
}

/// 无效 JSON：supervisor 丢弃非法行并继续读取下一条合法事件。
#[test]
fn invalid_json_line_is_skipped_and_next_event_read() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let (mut sidecar, _ready, _capabilities) =
        Sidecar::spawn_with_handshake(&collector_options(&py)).expect("handshake should succeed");

    // 发送一个带非法 payload 的命令（非对象），Python 端返回 error 事件而非崩溃。
    let command = CommandEnvelope::new(new_uuid_v7(), CommandType::Health, serde_json::json!([]));
    sidecar.send_command(&command).expect("send command");

    // 应能读到 error 事件（INVALID_COMMAND），说明 Rust 不会因单条坏消息崩溃。
    let event = sidecar
        .next_event(Duration::from_secs(5))
        .expect("should still read an event");
    assert_eq!(event.event_type, EventType::Error);
    assert_eq!(event.payload["code"], "INVALID_COMMAND");

    sidecar.shutdown(Duration::from_millis(500)).expect("clean shutdown");
}

/// 协议不兼容：sidecar 输出 protocol_version=2 时握手失败。
#[test]
fn incompatible_protocol_version_fails_handshake() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let script = concat!(
        "import sys\n",
        "print('{\"protocol_version\":2,\"request_id\":null,\"event_id\":\"",
        "019fbbd7-ea26-7b7c-b113-c89ac2788773\",\"type\":\"ready\",\"occurred_at\":\"",
        "2026-07-31T12:00:00.000Z\",\"payload\":{}}')\n",
        "sys.stdout.flush()\n",
        "import time; time.sleep(10)\n",
    );
    let options = SpawnOptions {
        program: py.to_path_buf(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_secs(3),
    };
    let err = Sidecar::spawn_with_handshake(&options).expect_err("handshake must fail");
    match err {
        SidecarError::Protocol(protocol::ProtocolError::UnsupportedVersion(2)) => {}
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

/// 退出码异常：sidecar 在握手前退出，返回退出码信息。
#[test]
fn early_exit_reports_exit_code() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let script = "import sys\nsys.exit(3)\n";
    let options = SpawnOptions {
        program: py.to_path_buf(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_secs(5),
    };
    let err = Sidecar::spawn_with_handshake(&options).expect_err("handshake must fail");
    match err {
        SidecarError::Exited { code, .. } => assert_eq!(code, Some(3)),
        other => panic!("expected Exited, got {other:?}"),
    }
}

/// 握手超时：sidecar 静默不输出，握手超时后返回可诊断错误。
#[test]
fn handshake_timeout_is_diagnostic() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let script = "import time\ntime.sleep(30)\n";
    let options = SpawnOptions {
        program: py.to_path_buf(),
        args: vec!["-u".into(), "-c".into(), script.into()],
        env: vec![("PYTHONUTF8".into(), "1".into())],
        cwd: None,
        handshake_timeout: Duration::from_millis(300),
    };
    let started = std::time::Instant::now();
    let err = Sidecar::spawn_with_handshake(&options).expect_err("handshake must fail");
    assert!(matches!(err, SidecarError::HandshakeTimeout(_)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// 采集流：发送 collect_user_posts 后能读到 started / user / post / done 事件。
#[test]
fn collect_stream_replays_fixture() {
    let Some(py) = python() else {
        eprintln!("python not found, skipping");
        return;
    };
    let (mut sidecar, _ready, _capabilities) =
        Sidecar::spawn_with_handshake(&collector_options(&py)).expect("handshake should succeed");

    let command = CommandEnvelope::new(
        new_uuid_v7(),
        CommandType::CollectUserPosts,
        serde_json::json!({"uid": "123", "max_pages": 1}),
    );
    sidecar.send_command(&command).expect("send command");

    let mut saw_started = false;
    let mut saw_post = false;
    let mut saw_done = false;
    let mut posts = 0;
    for _ in 0..200 {
        let event = sidecar
            .next_event(Duration::from_secs(5))
            .expect("should read event");
        match event.event_type {
            EventType::Started => saw_started = true,
            EventType::User => {}
            EventType::Post => {
                saw_post = true;
                posts += 1;
            }
            EventType::Checkpoint => {}
            EventType::Done => {
                saw_done = true;
                break;
            }
            _ => {}
        }
    }

    assert!(saw_started, "expected started event");
    assert!(saw_post, "expected at least one post event");
    assert!(posts > 0, "expected posts from fixture");
    assert!(saw_done, "expected done event");

    sidecar.shutdown(Duration::from_millis(500)).expect("clean shutdown");
}
