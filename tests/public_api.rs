use channel::{
    create_session, ActivityKind, CreateSessionWithConfig, HarnessType, SendMode, SendResult,
    SessionEvent, Status,
};

fn custom_harness(script: &str) -> HarnessType {
    HarnessType::Custom {
        command: "sh".into(),
        args: vec!["-c".into(), script.into()],
    }
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
    let mut session = create_session(
        custom_harness(r#"printf '%s\n' '{"type":"result","result":"ok"}'"#),
        None,
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
async fn custom_cli_events_are_normalized_and_aggregated() {
    let mut session = create_session(
        custom_harness(
            r#"printf '%s\n' '{"type":"tool_use","id":"read-1","name":"Read","input":{"file_path":"src/lib.rs"}}' '{"type":"tool_result","tool_use_id":"read-1","name":"Read","output":"ok"}' '{"type":"result","result":"done"}'"#,
        ),
        None,
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
async fn advanced_config_builder_and_runtime_initialization_work() {
    let cwd = std::env::current_dir().unwrap();
    let config = channel::SessionConfig::for_harness(custom_harness(
        r#"printf '%s\n' '{"type":"result","result":"configured"}'"#,
    ))
    .workspace(cwd.clone())
    .model("test-model")
    .network(channel::NetworkPolicy::Restricted)
    .approval(channel::ApprovalPolicy::AutoDeny)
    .provider_option("mode", serde_json::json!("test"))
    .build();

    let mut session = CreateSessionWithConfig(config, "hello").await.unwrap();
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

#[tokio::test(flavor = "current_thread")]
async fn initialized_runtime_applies_cwd_and_executable() {
    let cwd = std::env::current_dir().unwrap();
    let initialized = channel::initialize_with(
        channel::SessionConfig::for_harness(custom_harness(
            r#"printf '%s\n' '{"type":"result","result":"runtime"}'"#,
        ))
        .workspace(cwd.clone())
        .executable("/bin/sh")
        .build(),
    )
    .await
    .unwrap();
    assert_eq!(initialized.capabilities().effective.provider_resume, false);

    let mut session = initialized.create_session("hello").await.unwrap();
    assert_eq!(session.info().workspace.requested_cwd, cwd);
    assert_eq!(
        session.info().runtime.executable_path.as_deref(),
        Some(std::path::Path::new("/bin/sh"))
    );
    assert_eq!(
        session.info().backend.kind,
        channel::BackendKind::PlainCli
    );
    assert_eq!(wait_finished(&mut session).await.text, "runtime");
}

#[tokio::test(flavor = "current_thread")]
async fn resume_requires_an_effective_provider_capability() {
    let harness = channel::initialize_with(
        channel::SessionConfig::for_harness(custom_harness("exit 0")).build(),
    )
    .await
    .unwrap();
    assert_eq!(harness.capabilities().effective.provider_resume, false);

    assert!(matches!(
        channel::resume_session(
            harness,
            channel::ResumeTarget::channelSession("test".to_owned()),
        )
        .await,
        Err(channel::Error::UnsupportedCapability(_))
    ));
}
