use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serenity::all::{ChannelId, Client, CreateMessage, GatewayIntents, Http, Message, Ready};
use serenity::async_trait;
use serenity::gateway::ShardManager;
use serenity::prelude::{Context, EventHandler};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;

use crate::types::{Channel, ChannelFuture, NewMessage};

type HandlerFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
type MessageHandler = Arc<dyn Fn(NewMessage) -> HandlerFuture + Send + Sync>;
type ChatMetadataHandler =
    Arc<dyn Fn(String, String, Option<String>) -> HandlerFuture + Send + Sync>;

#[derive(Clone, Debug)]
pub struct DiscordChannelConfig {
    pub bot_token: String,
    pub reconnect_delay: Duration,
    pub assistant_name: String,
    pub prefix_outbound: bool,
}

pub struct DiscordChannel {
    config: DiscordChannelConfig,
    connected: AtomicBool,
    shutting_down: AtomicBool,
    flushing: AtomicBool,
    current_user_id: AtomicU64,
    shutdown_tx: watch::Sender<bool>,
    outgoing_queue: Mutex<VecDeque<(String, String)>>,
    typing_tasks: Mutex<HashMap<u64, JoinHandle<()>>>,
    http: Mutex<Option<Arc<Http>>>,
    shard_manager: Mutex<Option<Arc<ShardManager>>>,
    on_message: RwLock<Option<MessageHandler>>,
    on_chat_metadata: RwLock<Option<ChatMetadataHandler>>,
}

impl DiscordChannel {
    pub fn new(config: DiscordChannelConfig) -> Self {
        let (shutdown_tx, _) = watch::channel(false);
        Self {
            config,
            connected: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            flushing: AtomicBool::new(false),
            current_user_id: AtomicU64::new(0),
            shutdown_tx,
            outgoing_queue: Mutex::new(VecDeque::new()),
            typing_tasks: Mutex::new(HashMap::new()),
            http: Mutex::new(None),
            shard_manager: Mutex::new(None),
            on_message: RwLock::new(None),
            on_chat_metadata: RwLock::new(None),
        }
    }

    pub fn set_handlers<FMsg, FMsgFut, FMeta, FMetaFut>(
        &self,
        on_message: FMsg,
        on_chat_metadata: FMeta,
    ) where
        FMsg: Fn(NewMessage) -> FMsgFut + Send + Sync + 'static,
        FMsgFut: Future<Output = Result<(), String>> + Send + 'static,
        FMeta: Fn(String, String, Option<String>) -> FMetaFut + Send + Sync + 'static,
        FMetaFut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let on_message: MessageHandler =
            Arc::new(move |message: NewMessage| Box::pin(on_message(message)));
        let on_chat_metadata: ChatMetadataHandler = Arc::new(
            move |chat_jid: String, timestamp: String, name: Option<String>| {
                Box::pin(on_chat_metadata(chat_jid, timestamp, name))
            },
        );

        *self.on_message.write().expect("set_handlers message lock") = Some(on_message);
        *self
            .on_chat_metadata
            .write()
            .expect("set_handlers metadata lock") = Some(on_chat_metadata);
    }

    pub fn start(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            self.run_loop().await;
        })
    }

    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.shutdown_tx.send(true);
        if let Ok(mut tasks) = self.typing_tasks.try_lock() {
            for (_, handle) in tasks.drain() {
                handle.abort();
            }
        }

        if let Ok(guard) = self.shard_manager.try_lock()
            && let Some(manager) = guard.clone()
        {
            tokio::spawn(async move {
                manager.shutdown_all().await;
            });
        }
    }

    async fn run_loop(self: Arc<Self>) {
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        while !self.shutting_down.load(Ordering::SeqCst) {
            let intents = GatewayIntents::GUILDS
                | GatewayIntents::GUILD_MESSAGES
                | GatewayIntents::DIRECT_MESSAGES
                | GatewayIntents::MESSAGE_CONTENT;
            let handler = DiscordEventHandler {
                channel: self.clone(),
            };
            let mut client = match Client::builder(&self.config.bot_token, intents)
                .event_handler(handler)
                .await
            {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("failed to create Discord client: {err}");
                    tokio::select! {
                        _ = sleep(self.config.reconnect_delay) => {}
                        _ = wait_for_shutdown_signal(&mut shutdown_rx) => break,
                    }
                    continue;
                }
            };

            {
                let mut http_guard = self.http.lock().await;
                *http_guard = Some(client.http.clone());
                let mut shard_guard = self.shard_manager.lock().await;
                *shard_guard = Some(client.shard_manager.clone());
            }

            let mut start_fut = Box::pin(client.start_autosharded());
            tokio::select! {
                result = &mut start_fut => {
                    self.connected.store(false, Ordering::SeqCst);
                    if let Err(err) = result {
                        eprintln!("discord gateway exited with error: {err}");
                    }
                }
                _ = wait_for_shutdown_signal(&mut shutdown_rx) => {
                    self.connected.store(false, Ordering::SeqCst);
                    if let Some(manager) = self.shard_manager.lock().await.clone() {
                        manager.shutdown_all().await;
                    }
                }
            }
            self.clear_typing_tasks().await;

            {
                let mut http_guard = self.http.lock().await;
                *http_guard = None;
                let mut shard_guard = self.shard_manager.lock().await;
                *shard_guard = None;
            }

            if self.shutting_down.load(Ordering::SeqCst) || *shutdown_rx.borrow() {
                break;
            }
            sleep(self.config.reconnect_delay).await;
        }
    }

    async fn on_ready(&self, ready: Ready) {
        self.current_user_id
            .store(ready.user.id.get(), Ordering::SeqCst);
        self.connected.store(true, Ordering::SeqCst);
        if let Err(err) = self.flush_outgoing_queue().await {
            eprintln!("discord flush before inbound loop failed: {err}");
        }
    }

    async fn on_message(&self, message: Message) {
        let chat_jid = format_discord_channel_jid(message.channel_id.get());
        let timestamp = message.timestamp.to_string();
        let sender = message.author.id.get().to_string();
        let sender_name = message
            .author
            .global_name
            .clone()
            .unwrap_or_else(|| message.author.name.clone());
        let is_from_me = self.current_user_id.load(Ordering::SeqCst) == message.author.id.get();
        let is_bot_message = message.author.bot;
        let content = if message.content.trim().is_empty() {
            message
                .attachments
                .iter()
                .map(|attachment| attachment.url.clone())
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            message.content.clone()
        };

        let metadata_handler = self
            .on_chat_metadata
            .read()
            .expect("on_message metadata lock")
            .clone();
        if let Some(handler) = metadata_handler
            && let Err(err) = handler(chat_jid.clone(), timestamp.clone(), None).await
        {
            eprintln!("discord metadata handler failed: {err}");
        }

        let message_handler = self
            .on_message
            .read()
            .expect("on_message message lock")
            .clone();
        if let Some(handler) = message_handler {
            let inbound = NewMessage {
                id: message.id.get().to_string(),
                chat_jid,
                sender,
                sender_name,
                content,
                timestamp,
                is_from_me,
                is_bot_message,
            };
            if let Err(err) = handler(inbound).await {
                eprintln!("discord message handler failed: {err}");
            }
        }
    }

    fn decorate_outbound(&self, text: &str) -> String {
        if self.config.prefix_outbound {
            format!("{}: {text}", self.config.assistant_name)
        } else {
            text.to_string()
        }
    }

    async fn send_outbound_raw(&self, jid: &str, text: &str) -> Result<(), String> {
        let channel_id = parse_discord_channel_id(jid)
            .ok_or_else(|| format!("unsupported Discord channel JID: {jid}"))?;
        let http = self
            .http
            .lock()
            .await
            .clone()
            .ok_or_else(|| "discord channel is not connected".to_string())?;

        ChannelId::new(channel_id)
            .send_message(http.as_ref(), CreateMessage::new().content(text))
            .await
            .map(|_| ())
            .map_err(|err| format!("failed to send Discord message: {err}"))
    }

    async fn flush_outgoing_queue(&self) -> Result<(), String> {
        if !self.connected.load(Ordering::SeqCst) {
            return Ok(());
        }
        if self.flushing.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let result = async {
            loop {
                let next = { self.outgoing_queue.lock().await.pop_front() };
                let Some((jid, text)) = next else {
                    break;
                };

                if let Err(err) = self.send_outbound_raw(&jid, &text).await {
                    self.outgoing_queue.lock().await.push_front((jid, text));
                    return Err(err);
                }
            }
            Ok(())
        }
        .await;

        self.flushing.store(false, Ordering::SeqCst);
        result
    }

    async fn clear_typing_tasks(&self) {
        let mut tasks = self.typing_tasks.lock().await;
        for (_, handle) in tasks.drain() {
            handle.abort();
        }
    }
}

impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn owns_jid(&self, jid: &str) -> bool {
        parse_discord_channel_id(jid).is_some()
    }

    fn send_message<'a>(&'a self, jid: &'a str, text: &'a str) -> ChannelFuture<'a> {
        Box::pin(async move {
            let decorated = self.decorate_outbound(text);
            if !self.is_connected() {
                self.outgoing_queue
                    .lock()
                    .await
                    .push_back((jid.to_string(), decorated));
                return Ok(());
            }

            if let Err(err) = self.send_outbound_raw(jid, &decorated).await {
                eprintln!("discord outbound send failed for {jid}: {err}");
                self.outgoing_queue
                    .lock()
                    .await
                    .push_back((jid.to_string(), decorated));
            }
            Ok(())
        })
    }

    fn set_typing<'a>(&'a self, jid: &'a str, is_typing: bool) -> ChannelFuture<'a> {
        Box::pin(async move {
            let channel_id = parse_discord_channel_id(jid)
                .ok_or_else(|| format!("unsupported Discord channel JID: {jid}"))?;

            if !is_typing {
                let mut tasks = self.typing_tasks.lock().await;
                if let Some(handle) = tasks.remove(&channel_id) {
                    handle.abort();
                }
                return Ok(());
            }

            if !self.is_connected() {
                return Ok(());
            }

            let http = match self.http.lock().await.clone() {
                Some(http) => http,
                None => return Ok(()),
            };

            let mut tasks = self.typing_tasks.lock().await;
            if let Some(handle) = tasks.get(&channel_id)
                && !handle.is_finished()
            {
                return Ok(());
            }
            if let Some(handle) = tasks.remove(&channel_id) {
                handle.abort();
            }

            let mut shutdown_rx = self.shutdown_tx.subscribe();
            let handle = tokio::spawn(async move {
                loop {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                    if let Err(err) = ChannelId::new(channel_id)
                        .broadcast_typing(http.as_ref())
                        .await
                    {
                        eprintln!("failed to send Discord typing indicator: {err}");
                    }
                    tokio::select! {
                        _ = sleep(Duration::from_secs(8)) => {}
                        _ = wait_for_shutdown_signal(&mut shutdown_rx) => break,
                    }
                }
            });
            tasks.insert(channel_id, handle);
            Ok(())
        })
    }
}

struct DiscordEventHandler {
    channel: Arc<DiscordChannel>,
}

#[async_trait]
impl EventHandler for DiscordEventHandler {
    async fn ready(&self, _: Context, ready: Ready) {
        self.channel.on_ready(ready).await;
    }

    async fn message(&self, _: Context, message: Message) {
        self.channel.on_message(message).await;
    }
}

fn format_discord_channel_jid(channel_id: u64) -> String {
    format!("{channel_id}@discord")
}

fn parse_discord_channel_id(jid: &str) -> Option<u64> {
    let trimmed = jid.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = if let Some(value) = trimmed.strip_prefix("discord:") {
        value
    } else if let Some(value) = trimmed.strip_suffix("@discord") {
        value
    } else {
        trimmed
    };

    raw.trim().parse::<u64>().ok()
}

async fn wait_for_shutdown_signal(shutdown_rx: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown_rx.borrow() {
            return;
        }
        if shutdown_rx.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_discord_channel_id;

    #[test]
    fn parses_discord_jid_suffix() {
        assert_eq!(parse_discord_channel_id("123@discord"), Some(123));
    }

    #[test]
    fn parses_discord_prefix() {
        assert_eq!(parse_discord_channel_id("discord:456"), Some(456));
    }

    #[test]
    fn parses_raw_channel_id() {
        assert_eq!(parse_discord_channel_id("789"), Some(789));
    }

    #[test]
    fn rejects_invalid_channel_jid() {
        assert_eq!(parse_discord_channel_id("group@g.us"), None);
    }
}
