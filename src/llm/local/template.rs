//! Chat template rendering via minijinja.
//!
//! Both backends resolve a Jinja chat template (see the priority order in
//! [`render_chat`]'s callers) and render the conversation through the same
//! code path, so `explicit_template` overrides and error reporting behave
//! identically for GGUF and safetensors models.

use super::super::ChatMessage;
use crate::protocol::Error;
use serde::Serialize;

/// One entry of the template's `messages` array.
#[derive(Serialize)]
struct MessageContext<'a> {
    role: &'a str,
    content: &'a str,
}

/// The template context. Absent special tokens serialize to *undefined*
/// (rendering as empty strings under minijinja's lenient default) instead
/// of a JSON `null` that would render as `"none"`.
#[derive(Serialize)]
struct TemplateContext<'a> {
    messages: Vec<MessageContext<'a>>,
    add_generation_prompt: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    bos_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    eos_token: Option<&'a str>,
}

/// The template context keys every render provides. Templates that expect
/// `bos_token`/`eos_token` get them when the model advertises the values.
fn context_value(
    messages: &[ChatMessage],
    bos_token: Option<&str>,
    eos_token: Option<&str>,
) -> minijinja::Value {
    let context = TemplateContext {
        messages: messages
            .iter()
            .map(|message| MessageContext {
                role: message.role.as_str(),
                content: &message.content,
            })
            .collect(),
        add_generation_prompt: true,
        bos_token,
        eos_token,
    };
    minijinja::Value::from_serialize(&context)
}

/// A short excerpt of the template source, for error messages.
fn excerpt(source: &str) -> String {
    const LIMIT: usize = 160;
    if source.len() <= LIMIT {
        source.to_owned()
    } else {
        let mut cut = LIMIT;
        while !source.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &source[..cut])
    }
}

/// Renders `messages` through the chat `template`.
///
/// The render context contains `messages` (roles as lowercase
/// `system`/`user`/`assistant` strings), `add_generation_prompt = true` and,
/// when known, `bos_token`/`eos_token`.
///
/// A syntactically broken template (or one that fails at render time, as
/// some older model-provided templates do) yields [`Error::ProtocolError`]
/// carrying an excerpt of the template source so the caller can patch it
/// and pass it back via [`crate::LoadOptions::explicit_template`].
pub(crate) fn render_chat(
    template: &str,
    messages: &[ChatMessage],
    bos_token: Option<&str>,
    eos_token: Option<&str>,
) -> Result<String, Error> {
    let mut environment = minijinja::Environment::new();
    // Hugging Face templates routinely call Python string/list methods
    // (`content.split('</think>')[-1].strip()`, `x.startswith(...)`, ...);
    // the pycompat callback provides them.
    environment
        .set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    environment
        .add_template("chat", template)
        .map_err(|error| {
            Error::ProtocolError(format!(
                "invalid chat template ({error}): {}",
                excerpt(template)
            ))
        })?;
    let template = environment
        .get_template("chat")
        .expect("the chat template was just added");
    template
        .render(context_value(messages, bos_token, eos_token))
        .map_err(|error| {
            Error::ProtocolError(format!(
                "chat template rendering failed ({error}): {}",
                excerpt(template.source())
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::super::super::MessageRole;
    use super::*;

    const CHATML: &str = concat!(
        "{% for message in messages %}",
        "{{ '<|im_start|>' + message.role + '\n' + message.content + eos_token }}\n",
        "{% endfor %}",
        "{% if add_generation_prompt %}{{ '<|im_start|>assistant\n' }}{% endif %}"
    );

    fn history() -> Vec<ChatMessage> {
        vec![
            ChatMessage {
                role: MessageRole::System,
                ts_micros: 0,
                content: "be brief".to_owned(),
            },
            ChatMessage {
                role: MessageRole::User,
                ts_micros: 1,
                content: "hello".to_owned(),
            },
            ChatMessage {
                role: MessageRole::Assistant,
                ts_micros: 2,
                content: "hi".to_owned(),
            },
            ChatMessage {
                role: MessageRole::User,
                ts_micros: 3,
                content: "again".to_owned(),
            },
        ]
    }

    #[test]
    fn chatml_template_renders_all_roles_and_generation_prompt() {
        let rendered = render_chat(CHATML, &history(), None, Some("<|im_end|>")).unwrap();
        let expected = "<|im_start|>system\nbe brief<|im_end|>\n\
                        <|im_start|>user\nhello<|im_end|>\n\
                        <|im_start|>assistant\nhi<|im_end|>\n\
                        <|im_start|>user\nagain<|im_end|>\n\
                        <|im_start|>assistant\n";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn qwen_style_template_merges_system_into_first_user_message() {
        // Qwen's historical template folds the system prompt into the first
        // user turn instead of emitting a system role.
        let template = concat!(
            "{% if messages[0]['role'] == 'system' %}",
            "{% set system_message = messages[0]['content'] %}",
            "{% set messages = messages[1:] %}",
            "{% else %}",
            "{% set system_message = '' %}",
            "{% endif %}",
            "{% for message in messages %}",
            "{{ '<|im_start|>' + message.role + '\n' + ",
            "(message.content if not (loop.first and system_message) else system_message + '\n\n' + message.content) }}",
            "<|im_end|>\n",
            "{% endfor %}",
            "{% if add_generation_prompt %}{{ '<|im_start|>assistant\n' }}{% endif %}"
        );
        let rendered = render_chat(template, &history(), None, None).unwrap();
        let expected = "<|im_start|>user\nbe brief\n\nhello<|im_end|>\n\
                        <|im_start|>assistant\nhi<|im_end|>\n\
                        <|im_start|>user\nagain<|im_end|>\n\
                        <|im_start|>assistant\n";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn broken_template_reports_protocol_error_with_excerpt() {
        let error =
            render_chat("{% for message in messages %}", &history(), None, None).unwrap_err();
        assert!(
            matches!(error, Error::ProtocolError(ref message) if message.contains("chat template")),
            "got: {error:?}"
        );

        // A template that parses but fails at render time (a type error on
        // minijinja's lenient default) also reports a protocol error.
        let error = render_chat("{{ messages + 1 }}", &history(), None, None).unwrap_err();
        assert!(matches!(error, Error::ProtocolError(_)));
    }

    #[test]
    fn excerpt_truncates_on_char_boundary() {
        assert_eq!(excerpt("short"), "short");
        let long = "ä".repeat(200);
        let cut = excerpt(&long);
        assert!(cut.len() < long.len());
        assert!(cut.ends_with("..."));
    }
}
