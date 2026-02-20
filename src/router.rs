use std::fmt::{Display, Formatter};
use std::sync::OnceLock;

use regex::Regex;

use crate::types::{Channel, NewMessage};

#[derive(Debug, PartialEq, Eq)]
pub enum RouteError {
    NoChannel { jid: String },
    SendFailed(String),
}

impl Display for RouteError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoChannel { jid } => write!(f, "No channel for JID: {jid}"),
            RouteError::SendFailed(err) => write!(f, "Failed to send message: {err}"),
        }
    }
}

impl std::error::Error for RouteError {}

pub fn escape_xml(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }

    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn format_messages(messages: &[NewMessage], assistant_name: &str) -> String {
    let lines = messages
        .iter()
        .map(|message| {
            format!(
                "<message sender=\"{}\" time=\"{}\">{}</message>",
                escape_xml(&message.sender_name),
                message.timestamp,
                escape_xml(&message.content),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "You are {assistant_name}, the assistant for this group.\n\
Reply to the latest message in <messages> with one natural message.\n\
Rules:\n\
- Output only the final reply message.\n\
- Do not provide multiple tone/style options unless explicitly requested.\n\
- If asked your name or identity, identify yourself as \"{assistant_name}\".\n\
- Match the latest message language and keep it concise.\n\n\
<messages>\n{lines}\n</messages>"
    )
}

pub fn strip_internal_tags(text: &str) -> String {
    static INTERNAL_TAG_REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = INTERNAL_TAG_REGEX
        .get_or_init(|| Regex::new(r"(?s)<internal>.*?</internal>").expect("valid internal regex"));
    regex.replace_all(text, "").trim().to_string()
}

pub fn format_outbound(raw_text: &str) -> String {
    strip_internal_tags(raw_text)
}

pub fn find_channel<'a>(channels: &'a [&'a dyn Channel], jid: &str) -> Option<&'a dyn Channel> {
    channels
        .iter()
        .copied()
        .find(|channel| channel.owns_jid(jid))
}

pub async fn route_outbound(
    channels: &[&dyn Channel],
    jid: &str,
    text: &str,
) -> Result<(), RouteError> {
    let channel = channels
        .iter()
        .copied()
        .find(|candidate| candidate.owns_jid(jid) && candidate.is_connected())
        .ok_or_else(|| RouteError::NoChannel {
            jid: jid.to_string(),
        })?;

    channel
        .send_message(jid, text)
        .await
        .map_err(RouteError::SendFailed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct MockChannel {
        name: &'static str,
        connected: bool,
        jid_suffix: &'static str,
        sent: Arc<Mutex<Vec<(String, String)>>>,
        fail_send: bool,
    }

    impl MockChannel {
        fn new(name: &'static str, connected: bool, jid_suffix: &'static str) -> Self {
            Self {
                name,
                connected,
                jid_suffix,
                sent: Arc::new(Mutex::new(Vec::new())),
                fail_send: false,
            }
        }

        fn with_fail(mut self) -> Self {
            self.fail_send = true;
            self
        }
    }

    impl Channel for MockChannel {
        fn name(&self) -> &str {
            self.name
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn owns_jid(&self, jid: &str) -> bool {
            jid.ends_with(self.jid_suffix)
        }

        fn send_message<'a>(
            &'a self,
            jid: &'a str,
            text: &'a str,
        ) -> crate::types::ChannelFuture<'a> {
            Box::pin(async move {
                if self.fail_send {
                    return Err("send failed".to_string());
                }
                self.sent
                    .lock()
                    .expect("mock send lock")
                    .push((jid.to_string(), text.to_string()));
                Ok(())
            })
        }
    }

    #[test]
    fn escape_xml_escapes_critical_characters() {
        let escaped = escape_xml(r#"<tag attr="x">a&b</tag>"#);
        assert_eq!(escaped, "&lt;tag attr=&quot;x&quot;&gt;a&amp;b&lt;/tag&gt;");
    }

    #[test]
    fn format_messages_wraps_xml_messages() {
        let messages = vec![NewMessage {
            id: "1".to_string(),
            chat_jid: "group@g.us".to_string(),
            sender: "user".to_string(),
            sender_name: "Alice <QA>".to_string(),
            content: "hello & bye".to_string(),
            timestamp: "2026-02-19T00:00:00.000Z".to_string(),
            is_from_me: false,
            is_bot_message: false,
        }];

        let rendered = format_messages(&messages, "Andy");
        assert!(rendered.contains("You are Andy, the assistant for this group."));
        assert!(rendered.contains("Reply to the latest message in <messages>"));
        assert!(
            rendered.contains("If asked your name or identity, identify yourself as \"Andy\".")
        );
        assert!(rendered.contains(
            "<messages>\n<message sender=\"Alice &lt;QA&gt;\" time=\"2026-02-19T00:00:00.000Z\">hello &amp; bye</message>\n</messages>"
        ));
    }

    #[test]
    fn strip_internal_tags_removes_hidden_content() {
        let input = "before<internal>secret\ncontent</internal>after";
        assert_eq!(strip_internal_tags(input), "beforeafter");
    }

    #[tokio::test]
    async fn route_outbound_uses_connected_channel() {
        let disconnected = MockChannel::new("wa-disconnected", false, "@g.us");
        let connected = MockChannel::new("wa", true, "@g.us");

        let channels: Vec<&dyn Channel> = vec![&disconnected, &connected];
        let result = route_outbound(&channels, "123@g.us", "hello").await;
        assert!(result.is_ok());

        let sent = connected.sent.lock().expect("sent lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], ("123@g.us".to_string(), "hello".to_string()));
    }

    #[tokio::test]
    async fn route_outbound_errors_when_no_channel() {
        let channel = MockChannel::new("telegram", true, "@telegram");
        let channels: Vec<&dyn Channel> = vec![&channel];

        let result = route_outbound(&channels, "123@g.us", "hello").await;
        assert_eq!(
            result,
            Err(RouteError::NoChannel {
                jid: "123@g.us".to_string()
            })
        );
    }

    #[tokio::test]
    async fn route_outbound_propagates_send_failure() {
        let channel = MockChannel::new("wa", true, "@g.us").with_fail();
        let channels: Vec<&dyn Channel> = vec![&channel];

        let result = route_outbound(&channels, "123@g.us", "hello").await;
        assert_eq!(
            result,
            Err(RouteError::SendFailed("send failed".to_string()))
        );
    }
}
