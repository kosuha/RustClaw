use std::sync::Arc;

use crate::orchestrator::{OutboundSender, SendFuture};
use crate::router::route_outbound;
use crate::types::Channel;

pub struct ChannelOutboundSender {
    channels: Vec<Arc<dyn Channel>>,
}

impl ChannelOutboundSender {
    pub fn new(channels: Vec<Arc<dyn Channel>>) -> Self {
        Self { channels }
    }
}

impl OutboundSender for ChannelOutboundSender {
    fn send<'a>(&'a self, jid: &'a str, text: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            let channel_refs = self
                .channels
                .iter()
                .map(|channel| channel.as_ref() as &dyn Channel)
                .collect::<Vec<_>>();
            route_outbound(&channel_refs, jid, text)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn set_typing<'a>(&'a self, jid: &'a str, is_typing: bool) -> SendFuture<'a> {
        Box::pin(async move {
            let channel = self
                .channels
                .iter()
                .find(|candidate| candidate.owns_jid(jid) && candidate.is_connected())
                .ok_or_else(|| format!("no connected channel for JID: {jid}"))?;
            channel.set_typing(jid, is_typing).await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::types::ChannelFuture;

    #[derive(Clone)]
    struct MockChannel {
        connected: bool,
        jid_suffix: &'static str,
        sent: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl MockChannel {
        fn new(connected: bool, jid_suffix: &'static str) -> Self {
            Self {
                connected,
                jid_suffix,
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Channel for MockChannel {
        fn name(&self) -> &str {
            "mock"
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn owns_jid(&self, jid: &str) -> bool {
            jid.ends_with(self.jid_suffix)
        }

        fn send_message<'a>(&'a self, jid: &'a str, text: &'a str) -> ChannelFuture<'a> {
            Box::pin(async move {
                self.sent
                    .lock()
                    .expect("mock lock")
                    .push((jid.to_string(), text.to_string()));
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn sends_to_matching_connected_channel() {
        let channel = Arc::new(MockChannel::new(true, "@g.us"));
        let sender = ChannelOutboundSender::new(vec![channel.clone()]);

        let result = sender.send("group@g.us", "hello").await;
        assert!(result.is_ok());

        let sent = channel.sent.lock().expect("sent lock");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0], ("group@g.us".to_string(), "hello".to_string()));
    }

    #[tokio::test]
    async fn returns_error_when_no_channel_matches() {
        let channel = Arc::new(MockChannel::new(true, "@telegram"));
        let sender = ChannelOutboundSender::new(vec![channel]);

        let result = sender.send("group@g.us", "hello").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn typing_routes_to_matching_connected_channel() {
        let channel = Arc::new(MockChannel::new(true, "@g.us"));
        let sender = ChannelOutboundSender::new(vec![channel]);
        let result = sender.set_typing("group@g.us", true).await;
        assert!(result.is_ok());
    }
}
