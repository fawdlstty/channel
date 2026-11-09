//! 真机护栏：自研引擎无工具轮数上限（2026-11-09 主人定案「不应该存在
//! 这样的限制」）——超过旧版 50 轮预算的长批量任务照常推进到完成，全程
//! 不得出现任何轮数上限报错。
//!
//! 任务设计成必然超过旧上限：workspace 里放 60 个小文件，要求逐个 Read
//! （明令一轮只读一个、禁止并行），60 > 旧 50 轮预算；无上限后引擎一路
//! 读完并汇总落盘。
//!
//! 触发条件（环境变量；模型名缺省即跳过，CI 永不真跑）：
//! ```text
//! ENGINE_ROUND_LIMIT_BASE_URL  OpenAI 兼容端点（缺省 http://127.0.0.1:11434/v1，本机 Ollama）
//! ENGINE_ROUND_LIMIT_MODEL     模型名（复现主诉场景：eriktonon/deepseek-v3.1-flash-1017:9b）
//! ENGINE_ROUND_LIMIT_API_KEY   可选鉴权（Ollama 不需要）
//! ```
//!
//! 真机推理慢（9b 本地模型 60+ 轮可达 10-30 分钟），运行：
//! ```text
//! $env:ENGINE_ROUND_LIMIT_MODEL="eriktonon/deepseek-v3.1-flash-1017:9b"
//! cargo test --features ccswitch,history,local-gguf-cpu,llm,engine --test engine_round_limit -- --nocapture
//! ```

use std::collections::HashSet;
use std::time::Duration;

use channel::{
    ActivityKind, BackendSpec, EngineProvider, HarnessKind, Session, SessionConfig, SessionEvent,
    Status, ToolStatus,
};

/// 单事件空闲超时兜底（弱模型长思考轮也可能超过 5 分钟）。
const EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// fixture 文件数：必须 > 旧版每段 50 轮预算，覆盖「长任务不再被判死」。
const FILE_COUNT: usize = 60;

#[tokio::test(flavor = "multi_thread")]
async fn long_task_runs_without_round_cap() {
    let base_url = std::env::var("ENGINE_ROUND_LIMIT_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:11434/v1".to_owned());
    let Ok(model) = std::env::var("ENGINE_ROUND_LIMIT_MODEL") else {
        eprintln!("跳过：ENGINE_ROUND_LIMIT_MODEL 未设置（CI 不真跑）");
        return;
    };
    if model.is_empty() {
        eprintln!("跳过：ENGINE_ROUND_LIMIT_MODEL 为空");
        return;
    }
    let api_key = std::env::var("ENGINE_ROUND_LIMIT_API_KEY")
        .ok()
        .filter(|key| !key.is_empty());

    // fixture：临时 workspace + FILE_COUNT 个小文件（各含一行贡献值）。
    let workspace =
        std::env::temp_dir().join(format!("channel-round-limit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(workspace.join("notes")).expect("创建 fixture 目录");
    for index in 0..FILE_COUNT {
        std::fs::write(
            workspace.join("notes").join(format!("note-{index:02}.txt")),
            format!("文件 {index}：贡献值 = {}\n", index * 2),
        )
        .expect("写 fixture 文件");
    }

    // 会话：引擎渠道直连端点；context_window 不设（0 = 关掉水位压缩），
    // 把本护栏隔离到纯「无轮数上限推进」路径；full_access 免权限挂起
    // （测试目录固定）。
    let mut config = SessionConfig::for_kind(HarnessKind::BlackAgentEngine);
    config.backend = BackendSpec::Engine {
        provider: EngineProvider {
            base_url: base_url.clone(),
            api_key,
        },
    };
    config.set_model(Some(model.clone()));
    config.set_workspace(Some(workspace.clone()));
    config.set_full_access(true);
    // 引擎按 permission_mode 字符串判权限矩阵（None 默认 Build：写类
    // 工具一律询问）。护栏验收长任务推进而非权限链路，必须显式 yolo——
    // 否则模型开始写 summary.txt 时 Ask 挂起、无应答即全链冻结
    // （2026-11-08 真机踩坑）。
    config.set_permission_mode(Some("yolo".to_owned()));
    config.set_observability(false);

    let prompt = format!(
        "workspace 的 notes/ 目录下有 {FILE_COUNT} 个 note-XX.txt 文件。请用 Read 工具逐个读取它们：\
         严格一次只读一个文件，禁止在同一轮并行发起多个 Read，也不许用 Bash/Glob/Grep 批量偷看内容。\
         每读一个文件就记下其中的“贡献值”。全部读完后，用 Write 工具把 {FILE_COUNT} 个贡献值汇总写入 \
         notes/summary.txt（每行格式：文件名=贡献值），最后回复 DONE。\
         这是无人值守的机械任务：禁止调用 AskUserQuestion，不要向用户提问，\
         遇到任何不确定都按最合理的方式继续执行。"
    );

    let start = std::time::Instant::now();
    let mut session = Session::create(config, prompt).await.expect("引擎会话创建");

    let mut compaction_count = 0usize;
    let mut read_activities: HashSet<String> = HashSet::new();
    let mut answer = String::new();
    // 观测计数：区分「模型在长考（活着但安静）」与「引擎真冻结」。
    let mut delta_events = 0usize;
    let mut delta_chars = 0usize;
    let mut other_events = 0usize;
    loop {
        let event = tokio::time::timeout(EVENT_IDLE_TIMEOUT, session.wait_event())
            .await
            .expect("事件空闲超时")
            .expect("事件流出错");
        match event {
            Some(SessionEvent::Activity(activity)) => {
                if matches!(activity.kind, ActivityKind::ReadFile)
                    && activity.status == ToolStatus::Completed
                {
                    let fresh = read_activities.insert(activity.display.clone());
                    // 心跳：每 10 个新文件打印一次，区分“慢而活着”与“真挂死”。
                    if fresh && read_activities.len().is_multiple_of(10) {
                        println!(
                            "进度：已读 {} 个文件（耗时 {:?}）",
                            read_activities.len(),
                            start.elapsed()
                        );
                    }
                }
            }
            Some(SessionEvent::UserInput { request }) => {
                // 无人值守自动应答：有选项选第一个，纯文本题回“继续”。
                let mut answers = std::collections::BTreeMap::new();
                for question in &request.questions {
                    let reply = question
                        .options
                        .first()
                        .map(|option| option.label.clone())
                        .unwrap_or_else(|| "继续".to_owned());
                    println!(
                        "自动应答问题（id={}，题干={:?}）→ {reply:?}",
                        question.id, question.question
                    );
                    answers.insert(question.id.clone(), vec![reply]);
                }
                session
                    .respond_user_input(&request.id, answers)
                    .await
                    .expect("自动应答失败");
            }
            Some(SessionEvent::CompactionBoundary { payload }) => {
                compaction_count += 1;
                println!(
                    "压缩边界 #{compaction_count}: preCompactTokenCount={:?} 摘要前 200 字={:?}",
                    payload.get("preCompactTokenCount"),
                    payload
                        .get("summaryText")
                        .and_then(|value| value.as_str())
                        .map(|text| text.chars().take(200).collect::<String>()),
                );
            }
            Some(SessionEvent::Message(message)) => {
                delta_events += 1;
                delta_chars += message.text.chars().count();
                if delta_events.is_multiple_of(400) {
                    println!(
                        "[心跳] 流增量 {delta_events} 条 / {delta_chars} 字（含思考）@ {:?}",
                        start.elapsed()
                    );
                }
                if !message.reasoning {
                    answer.push_str(&message.text);
                }
            }
            Some(SessionEvent::Finished(finish)) => {
                assert_eq!(
                    finish.status,
                    Status::Completed,
                    "turn 应完成而非失败（失败=护栏未生效）: {finish:?}"
                );
                break;
            }
            Some(_) => {
                other_events += 1;
                if other_events.is_multiple_of(100) {
                    println!(
                        "[心跳] 其他事件累计 {other_events} 条 @ {:?}",
                        start.elapsed()
                    );
                }
            }
            None => panic!("事件流提前中断（未收到 Finished）"),
        }
    }

    // 断言一：任务确实推进超过旧版 50 轮预算（>50 个不同的 Read 活动）——
    // 「无轮数上限」语义被真实验证而非任务碰巧太短。
    assert!(
        read_activities.len() > 50,
        "只观察到 {} 个 Read 活动：模型并行化/未按令逐个读，长任务路径未被覆盖，本护栏不构成有效验收（请检查任务约束或模型纪律后重跑）",
        read_activities.len()
    );
    // 断言二：任务产物落盘且覆盖全部文件（无上限的引擎真的把长任务干完了）。
    let summary_path = workspace.join("notes").join("summary.txt");
    let summary =
        std::fs::read_to_string(&summary_path).expect("summary.txt 未落盘：长任务没有推进到汇总");
    assert_eq!(
        summary.lines().count(),
        FILE_COUNT,
        "summary.txt 应覆盖全部 {FILE_COUNT} 个文件（实际 {} 行）:\n{summary}",
        summary.lines().count()
    );

    println!(
        "护栏通过：耗时 {:?}，Read 活动 {} 个（超过旧 50 轮预算），压缩边界 {} 次，最终回复 {answer}",
        start.elapsed(),
        read_activities.len(),
        compaction_count
    );
    let _ = std::fs::remove_dir_all(&workspace);
}
