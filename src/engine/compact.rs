//! Context compaction for the engine (ZCode `compact_boundary` semantics):
//! water-level observation, model-generated summary, message-log
//! replacement and the four-key boundary payload the host already
//! transcribes into its store.

use crate::llm::{ChatCompletionsClient, ChatMessage, MessageRole, StreamUsage};

/// 水位触发线：`context_window − min(max_output, 21000) − 13000`（与
/// BlackAgent 宿主既有公式一致）。
pub fn compaction_watermark(context_window: u64, max_output: Option<u64>) -> u64 {
    context_window.saturating_sub(max_output.unwrap_or(21000).min(21000)).saturating_sub(13_000)
}

/// Estimates prompt usage from the message log (the fallback path when
/// the provider streams no usage chunk). ~4 bytes per token, ASCII-biased;
/// CJK runs heavier, which errs toward compacting earlier — the safe
/// direction. `None` on an empty log.
pub fn estimate_usage_from_log(messages: &[ChatMessage]) -> Option<StreamUsage> {
    let chars: u64 = messages.iter().map(|message| message.content.len() as u64).sum();
    if chars == 0 {
        None
    } else {
        Some(StreamUsage {
            prompt_tokens: chars.div_ceil(4),
            completion_tokens: 0,
        })
    }
}

/// One compaction decision outcome.
pub enum CompactionOutcome {
    /// Below the watermark — nothing to do.
    NotNeeded,
    /// Compaction ran; the boundary payload follows the ZCode four-key
    /// schema (boundaryId/summaryMessageIds/preCompactTokenCount/traceId).
    Completed { payload: serde_json::Value },
    /// The summary request failed (turn continues uncompacted).
    Failed(String),
}

/// 水位压缩的摘要指令（保持既有行为：目标/已完成/进行中/决定与失败）。
const SUMMARY_INSTRUCTION: &str = "你负责把一段 agent 工作对话压缩为后续工作可用的摘要。保留：当前任务目标、已完成的步骤与产物（含文件路径）、进行中的步骤、关键决定与失败尝试。不要寒暄，直接输出摘要正文。";

/// Runs a compaction pass over the client's message log when the usage
/// crosses the watermark. The summary request itself goes through the same
/// endpoint (one extra model round, no tools).
pub async fn maybe_compact(
    client: &mut ChatCompletionsClient,
    usage: StreamUsage,
    context_window: u64,
    max_output: Option<u64>,
) -> CompactionOutcome {
    let watermark = compaction_watermark(context_window, max_output);
    if context_window == 0 || usage.prompt_tokens < watermark {
        return CompactionOutcome::NotNeeded;
    }
    let boundary_id = format!("compact-{}", crate::llm::unix_micros());
    let summarized_count = client.messages().len();
    match run_summary(client, SUMMARY_INSTRUCTION).await {
        Ok(summary) => {
            replace_history_with_summary(client, &summary);
            CompactionOutcome::Completed {
                payload: boundary_payload(&boundary_id, usage.prompt_tokens, summarized_count, &summary),
            }
        }
        Err(err) => CompactionOutcome::Failed(err),
    }
}

/// The four-key boundary payload the host transcribes (ZCode schema);
/// `summaryText` rides along because the engine's message log is in-process
/// and nothing else carries the summary body to the host store. Key set
/// mirrors the watermark path — the host parses both with the same
/// `CompactionBoundary` shape.
fn boundary_payload(
    boundary_id: &str,
    pre_compact_tokens: u64,
    summarized_message_count: usize,
    summary: &str,
) -> serde_json::Value {
    serde_json::json!({
        "boundaryId": boundary_id,
        "trigger": "auto",
        "summaryMessageIds": [],
        "preCompactTokenCount": pre_compact_tokens,
        "summarizedMessageCount": summarized_message_count,
        "traceId": boundary_id,
        "summaryText": summary,
    })
}

/// 摘要请求转写的每条消息上限（字符）：超长工具结果按头截断并注明，
/// 防小上下文端点（本地 GGUF）摘要请求先爆上下文。
const TRANSCRIPT_MESSAGE_CAP: usize = 1_000;
/// 摘要请求转写的整体上限（字符）：超出时丢弃更早的消息（保最新）。
const TRANSCRIPT_TOTAL_CAP: usize = 40_000;
/// 参与转写的最大消息条数（尾窗，与既有行为一致）。
const TRANSCRIPT_MAX_MESSAGES: usize = 80;

/// Builds the bounded transcript: the opening user directive pinned at the
/// top, then the last [`TRANSCRIPT_MAX_MESSAGES`] messages as plain
/// `[role] body` lines, each body capped at [`TRANSCRIPT_MESSAGE_CAP`]
/// chars, oldest dropped until the whole transcript fits
/// [`TRANSCRIPT_TOTAL_CAP`]. Oldest-first order (chat chronology), so the
/// summarizer reads the story forward.
///
/// Why the pin: the tail window alone drops the original task directive on
/// long tasks — the summarizer then loses the backlog and the resumed turn
/// wrongly declares the task finished (real-machine guardrail finding,
/// 2026-11-08).
fn build_transcript(messages: &[ChatMessage]) -> String {
    let opening_directive = messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| {
            format!("[user]（任务原始指令）\n{}\n", truncate_body(&message.content))
        });
    let first_user = messages
        .iter()
        .position(|message| message.role == MessageRole::User);

    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (index, message) in messages.iter().enumerate().rev() {
        if lines.len() >= TRANSCRIPT_MAX_MESSAGES {
            break;
        }
        if Some(index) == first_user {
            // 原始指令已钉头，不再占用尾窗名额。
            continue;
        }
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        let line = format!("[{role}] {}\n", truncate_body(&message.content));
        if total + line.len() > TRANSCRIPT_TOTAL_CAP && !lines.is_empty() {
            break;
        }
        total += line.len();
        lines.push(line);
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(directive) = opening_directive {
        parts.push(directive);
    }
    parts.push(lines.concat());
    parts.concat()
}

/// 按字符数截断（不按字节，避免切在多字节字符中间）；超出部分以省略
/// 标记收尾。
fn truncate_body(content: &str) -> String {
    if content.chars().count() <= TRANSCRIPT_MESSAGE_CAP {
        return content.to_owned();
    }
    let head: String = content.chars().take(TRANSCRIPT_MESSAGE_CAP).collect();
    format!("{head}…（已截断）")
}

/// Asks the model for a rolling summary of the conversation so far. The
/// instruction decides the summary shape (generic vs task-state breakdown
/// for the round-limit path). Runs through a throwaway one-shot client
/// sharing the endpoint, so the working log is untouched.
async fn run_summary(
    client: &ChatCompletionsClient,
    instruction: &str,
) -> Result<String, String> {
    let transcript = build_transcript(&client.messages());
    let mut summarizer = ChatCompletionsClient::new(
        client.core.base_url.clone(),
        client.core.api_key.clone(),
    );
    summarizer.core.model = client.core.model.clone();
    summarizer.set_system_prompt(instruction);
    let answer = summarizer
        .chat(format!("对话记录：\n{transcript}\n\n请输出结构化摘要。"))
        .await
        .map_err(|err| format!("压缩摘要请求失败: {err}"))?;
    Ok(answer)
}

/// Replaces the conversation history with: original system prompt + the
/// compaction summary as a user message. (The log is the engine session;
/// this is the context-window reset.)
///
/// 系统提示不在这里复制进 fresh：[`crate::llm::ClientCore::set_messages`]
/// 的 potato 语义本来就保留日志里已有的 system 条目（原位置在首位），
/// 再补一份会得到 [system, system, user]——第二条第 system 撞 Qwen 系
/// 严格 chat template 的 "System message must be at the beginning"（上游
/// HTTP 500，且压缩后每轮请求都带，整个会话报废；2026-11-08 真机报障）。
fn replace_history_with_summary(client: &mut ChatCompletionsClient, summary: &str) {
    let fresh = vec![ChatMessage::user(format!(
        "（上文对话已被压缩。以下为工作摘要，请基于它继续任务。）\n\n{summary}"
    ))];
    client.set_messages(fresh);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: MessageRole, content: &str) -> ChatMessage {
        let mut message = ChatMessage::user(content.to_owned());
        message.role = role;
        message
    }

    #[test]
    fn boundary_payload_carries_host_schema_keys() {
        let payload = boundary_payload("compact-1", 4321, 7, "摘要正文");
        assert_eq!(payload["boundaryId"], "compact-1");
        assert_eq!(payload["trigger"], "auto");
        assert_eq!(payload["preCompactTokenCount"], 4321);
        assert_eq!(payload["summarizedMessageCount"], 7);
        assert_eq!(payload["traceId"], "compact-1");
        assert_eq!(payload["summaryText"], "摘要正文");
        assert_eq!(payload["summaryMessageIds"], serde_json::json!([]));
    }

    #[test]
    fn truncate_body_keeps_char_boundaries() {
        assert_eq!(truncate_body("短文本"), "短文本");
        let long = "喵".repeat(TRANSCRIPT_MESSAGE_CAP + 50);
        let cut = truncate_body(&long);
        // 头部完整保留 TRANSCRIPT_MESSAGE_CAP 个字符，尾接截断标记。
        assert!(cut.starts_with(&"喵".repeat(TRANSCRIPT_MESSAGE_CAP)));
        assert!(cut.ends_with("…（已截断）"));
        assert!(cut.chars().count() < long.chars().count());
    }

    #[test]
    fn transcript_bounds_messages_bodies_and_total() {
        // 无 user 消息 → 无钉头，纯尾窗行为与旧版一致。
        let messages: Vec<ChatMessage> = (0..200)
            .map(|index| {
                message(
                    MessageRole::Tool,
                    &format!("msg{index:03}-{}", "x".repeat(500)),
                )
            })
            .collect();
        let transcript = build_transcript(&messages);
        // 尾窗上限：最多 TRANSCRIPT_MAX_MESSAGES 行。
        let line_count = transcript.lines().count();
        assert!(line_count <= TRANSCRIPT_MAX_MESSAGES);
        // 整体上限兜底。
        assert!(transcript.len() <= TRANSCRIPT_TOTAL_CAP + 2_000);
        // 最新消息保留（chronology 尾窗），最老的被丢掉（零填充标记避免子串误判）。
        assert!(transcript.contains("msg199-"));
        assert!(!transcript.contains("msg000-"));
        // 每条正文按字符截断。
        let long = message(MessageRole::User, &"测".repeat(5_000));
        let single = build_transcript(std::slice::from_ref(&long));
        assert!(single.contains("…（已截断）"));
        assert!(single.chars().count() < 5_000);
    }

    #[test]
    fn transcript_pins_opening_user_directive() {
        // 120 条工具消息把最初指令裁出尾窗，但头部必须钉住它——否则摘要
        // 丢失待办清单，续跑方误判任务已完成（2026-11-08 真机踩坑回归）。
        let mut messages: Vec<ChatMessage> = Vec::new();
        messages.push(message(MessageRole::User, "总目标：读 60 个文件并汇总落盘"));
        for index in 0..120 {
            messages.push(message(
                MessageRole::Tool,
                &format!("msg{index:03}-{}", "y".repeat(300)),
            ));
        }
        let transcript = build_transcript(&messages);
        assert!(transcript.contains("（任务原始指令）"));
        assert!(transcript.contains("总目标：读 60 个文件并汇总落盘"));
        // 最新现场仍在尾窗。
        assert!(transcript.contains("msg119-"));
        // 原始指令只钉一次，不重复占用尾窗名额。
        assert_eq!(transcript.matches("总目标：读 60 个文件并汇总落盘").count(), 1);
    }

    #[test]
    fn watermark_formula_matches_host() {
        // 64k 窗、8k 输出：64000 − 8000 − 13000 = 43000。
        assert_eq!(compaction_watermark(64_000, Some(8_000)), 43_000);
        // 0 窗交给调用方短路，公式本身不恐慌。
        assert_eq!(compaction_watermark(0, None), 0);
    }

    /// 压缩重排后日志必须恰好一条 system 且在首位：旧版把 system 复制进
    /// fresh 再经 set_messages 的 potato 保留语义叠加成 [system, system,
    /// user]，第二条第 system 撞 Qwen 系严格模板（HTTP 500，2026-11-08
    /// 真机报障回归）。
    #[test]
    fn replace_history_keeps_single_leading_system() {
        let mut client = ChatCompletionsClient::new("http://localhost", None);
        client.set_system_prompt("主线系统提示");
        client.core.push_user_message("任务");
        client.core.push_assistant_message("进行中".to_owned());
        client.core.push_user_message("继续");

        replace_history_with_summary(&mut client, "摘要正文");
        let messages = client.messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[0].content, "主线系统提示");
        assert_eq!(messages[1].role, MessageRole::User);
        assert!(messages[1].content.contains("摘要正文"));
    }
}
