use channel::{ActivityKind, HarnessKind, SendMode, SendResult, SessionEvent, Status};

fn script_config(script: &str) -> channel::SessionConfig {
    let mut config = channel::SessionConfig::for_kind(HarnessKind::ClaudeCode);
    #[cfg(windows)]
    let (command, args) = (
        shell_executable(),
        vec!["-NoProfile".into(), "-Command".into(), script.into()],
    );
    #[cfg(not(windows))]
    let (command, args) = ("sh".into(), vec!["-c".into(), script.into()]);
    config.backend = channel::BackendSpec::PlainCli { command, args };
    config
}

#[cfg(windows)]
fn shell_executable() -> String {
    std::path::Path::new(&std::env::var_os("WINDIR").unwrap())
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
        .to_string_lossy()
        .into_owned()
}

#[cfg(not(windows))]
fn shell_executable() -> &'static str {
    "/bin/sh"
}

async fn wait_finished(session: &mut channel::Session) -> channel::Finish {
    loop {
        match session.wait_event().await.unwrap() {
            Some(SessionEvent::Finished(finish)) => return finish,
            Some(_) => {}
            None => panic!("backend ended without a final result"),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn simple_create_and_wait_send_work() {
    let mut session = channel::Session::create(
        script_config(r#"Write-Output '{"type":"result","result":"ok"}'"#),
        "hello",
    )
    .await
    .unwrap();
    assert_eq!(wait_finished(&mut session).await.status, Status::Completed);
    assert_eq!(session.result().unwrap().text, "ok");

    let result = session.send("again", SendMode::Wait).await.unwrap();
    assert!(matches!(result, SendResult::Finished(finish) if finish.text == "ok"));
}

#[tokio::test(flavor = "current_thread")]
async fn plain_cli_events_are_normalized_and_aggregated() {
    let mut session = channel::Session::create(
        script_config(
            r#"Write-Output '{"type":"tool_use","id":"read-1","name":"Read","input":{"file_path":"src/lib.rs"}}'; Write-Output '{"type":"tool_result","tool_use_id":"read-1","name":"Read","output":"ok"}'; Write-Output '{"type":"result","result":"done"}'"#,
        ),
        "hello",
    )
    .await
    .unwrap();
    while !matches!(
        session.wait_event().await.unwrap(),
        Some(SessionEvent::Finished(_))
    ) {}

    let activity = session.activity();
    assert_eq!(activity.counts.total, 2);
    assert!(activity
        .items
        .iter()
        .any(|item| item.kind == ActivityKind::ReadFile));
    assert!(session.capabilities().observed.tool_calls);
    assert_eq!(session.result().unwrap().text, "done");
}

#[tokio::test(flavor = "current_thread")]
async fn advanced_config_and_runtime_initialization_work() {
    let cwd = std::env::current_dir().unwrap();
    let mut config = script_config(r#"Write-Output '{"type":"result","result":"configured"}'"#);
    config.set_workspace(Some(cwd.clone()));
    config.set_model(Some("test-model".to_owned()));
    config.set_reasoning_effort(Some(channel::ReasoningEffort::High));
    config.security.network = channel::NetworkPolicy::Restricted;
    config.security.approval = channel::ApprovalPolicy::AutoDeny;
    config
        .provider_options
        .insert("mode".to_owned(), serde_json::json!("test"));
    assert_eq!(config.get_workspace().as_deref(), Some(cwd.as_path()));
    assert_eq!(config.get_model().as_deref(), Some("test-model"));
    assert_eq!(
        config.get_reasoning_effort(),
        Some(channel::ReasoningEffort::High)
    );

    let mut session = channel::Session::create(config, "hello").await.unwrap();
    assert_eq!(session.info().workspace.requested_cwd, cwd);
    assert_eq!(
        session.info().model.requested.as_deref(),
        Some("test-model")
    );
    assert_eq!(
        session.info().security.approval,
        channel::ApprovalPolicy::AutoDeny
    );
    assert_eq!(wait_finished(&mut session).await.text, "configured");
}

#[test]
fn session_config_accessors_support_optional_values() {
    let mut config = channel::SessionConfig::for_kind(channel::HarnessKind::Codex);
    assert_eq!(config.get_workspace(), None);
    assert_eq!(config.get_observability(), None);
    assert_eq!(config.get_model(), None);
    assert_eq!(config.get_reasoning_effort(), None);

    config.set_workspace(Some("/tmp/channel".into()));
    config.set_observability(false);
    config.set_model(Some("test-model".to_owned()));
    config.model.available_models = vec!["test-model".to_owned()];
    config.set_reasoning_effort(Some(channel::ReasoningEffort::High));
    config.set_full_access(true);
    config.set_switch_provider(channel::SwitchProvider::CcSwitch, "switch-key");

    assert_eq!(
        config.get_workspace().as_deref(),
        Some(std::path::Path::new("/tmp/channel"))
    );
    assert_eq!(config.get_observability(), Some(false));
    assert_eq!(config.get_model().as_deref(), Some("test-model"));
    assert_eq!(
        config.get_reasoning_effort(),
        Some(channel::ReasoningEffort::High)
    );
    assert_eq!(
        config.security.approval,
        channel::ApprovalPolicy::AutoApprove
    );
    assert_eq!(
        config.security.permissions.filesystem.mode,
        channel::FilesystemAccess::FullHost
    );
    assert_eq!(
        config.get_switch_key(channel::SwitchProvider::CcSwitch),
        Some("switch-key")
    );
    config.clear_switch_provider(channel::SwitchProvider::CcSwitch);
    assert_eq!(
        config.get_switch_key(channel::SwitchProvider::CcSwitch),
        None
    );

    config.set_workspace(None);
    config.observability = None;
    assert_eq!(config.get_workspace(), None);
    assert_eq!(config.get_observability(), None);
}

#[tokio::test(flavor = "current_thread")]
async fn initialized_runtime_applies_cwd_and_executable() {
    let cwd = std::env::current_dir().unwrap();
    let mut config = script_config(r#"Write-Output '{"type":"result","result":"runtime"}'"#);
    config.set_workspace(Some(cwd.clone()));
    let executable = std::path::PathBuf::from(shell_executable());
    config.runtime.process.executable = Some(executable.clone());
    let harness = channel::Harness::initialize_with(config.clone())
        .await
        .unwrap();
    assert_eq!(harness.capabilities().effective.provider_resume, false);
    assert!(harness.available_models().is_empty());

    let mut session = channel::Session::create(config, "hello").await.unwrap();
    assert_eq!(session.info().workspace.requested_cwd, cwd);
    assert_eq!(
        session.info().runtime.executable_path.as_deref(),
        Some(executable.as_path())
    );
    assert_eq!(session.info().backend.kind, channel::BackendKind::PlainCli);
    assert_eq!(wait_finished(&mut session).await.text, "runtime");
}

#[tokio::test(flavor = "current_thread")]
async fn resume_requires_an_effective_provider_capability() {
    let harness = channel::Harness::initialize_with(script_config("exit 0"))
        .await
        .unwrap();
    assert_eq!(harness.capabilities().effective.provider_resume, false);

    assert!(matches!(
        harness
            .resume(channel::ResumeTarget::ChannelDefault("test".to_owned()),)
            .await,
        Err(channel::Error::UnsupportedCapability(_))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn resume_target_reaches_cli_session_arguments() {
    let mut config = channel::SessionConfig::for_kind(channel::HarnessKind::ClaudeCode);
    #[cfg(windows)]
    let (command, args) = (
        shell_executable(),
        vec![
            "-NoProfile".into(),
            "-Command".into(),
            r#"Write-Output '{"type":"result","result":"sid-{session_id}"}'"#.into(),
        ],
    );
    #[cfg(not(windows))]
    let (command, args) = (
        "sh".to_owned(),
        vec![
            "-c".into(),
            r#"echo '{"type":"result","result":"sid-{session_id}"}'"#.into(),
        ],
    );
    config.backend = channel::BackendSpec::PlainCli { command, args };
    config.conversation.mode = channel::ConversationSpec::Resume(
        channel::ResumeTarget::ProviderSession { id: "42".to_owned() },
    );

    let mut session = channel::Session::create(config, "continue")
        .await
        .unwrap();
    assert_eq!(
        session.info().visibility.resumability,
        channel::Resumability::ProviderResume
    );
    assert_eq!(wait_finished(&mut session).await.text, "sid-42");
    assert_eq!(
        session.info().external_ids.provider_session_id.as_deref(),
        Some("42")
    );
}
