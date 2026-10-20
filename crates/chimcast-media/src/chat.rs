//! Read-only Twitch chat over IRC.
//!
//! Twitch lets anyone read a channel's chat anonymously (nick `justinfanNNN`)
//! over plain IRC, so the chat panel needs no login, no TLS library and no
//! browser: one thread, one socket, a few kilobytes of messages.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SERVER: &str = "irc.chat.twitch.tv:6667";
/// Messages kept for the panel; older ones are dropped.
const KEEP: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub struct ChatMessage {
    pub name: String,
    /// The user's chosen name color, if they set one.
    pub color: Option<[u8; 3]>,
    pub text: String,
    /// A `/me` action, shown in the user's color.
    pub action: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChatState {
    Connecting,
    Joined,
    Disconnected(String),
}

struct Shared {
    messages: Mutex<VecDeque<ChatMessage>>,
    state: Mutex<ChatState>,
    stop: AtomicBool,
    /// Bumped on every new message so the UI can scroll.
    count: Mutex<u64>,
}

pub struct TwitchChat {
    channel: String,
    shared: Arc<Shared>,
}

impl TwitchChat {
    /// Starts reading `channel` (with or without `#`, any case) in the background,
    /// reconnecting on its own if the connection drops.
    pub fn connect(channel: &str) -> Self {
        let channel = channel.trim().trim_start_matches('#').to_ascii_lowercase();
        let shared = Arc::new(Shared {
            messages: Mutex::new(VecDeque::new()),
            state: Mutex::new(ChatState::Connecting),
            stop: AtomicBool::new(false),
            count: Mutex::new(0),
        });
        let (ch, sh) = (channel.clone(), shared.clone());
        let _ = std::thread::Builder::new()
            .name("twitch-chat".into())
            .spawn(move || run(&ch, &sh));
        Self { channel, shared }
    }

    pub fn channel(&self) -> &str {
        &self.channel
    }

    pub fn state(&self) -> ChatState {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        self.shared
            .messages
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    pub fn count(&self) -> u64 {
        *self.shared.count.lock().unwrap()
    }
}

impl Drop for TwitchChat {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn run(channel: &str, shared: &Shared) {
    let mut backoff = 1;
    while !shared.stop.load(Ordering::Relaxed) {
        *shared.state.lock().unwrap() = ChatState::Connecting;
        let err = match session(channel, shared) {
            Ok(()) => "connection closed".to_string(),
            Err(e) => e.to_string(),
        };
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        // It was working: start the retry delays over.
        if *shared.state.lock().unwrap() == ChatState::Joined {
            backoff = 1;
        }
        log::warn!("twitch chat #{channel}: {err}");
        *shared.state.lock().unwrap() = ChatState::Disconnected(err);
        for _ in 0..backoff * 10 {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(30);
    }
}

fn session(channel: &str, shared: &Shared) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(SERVER)?;
    // Wake up regularly to notice a stop request; Twitch pings every ~5 minutes.
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let nick = format!("justinfan{}", 10_000 + std::process::id() % 80_000);
    write!(
        stream,
        "CAP REQ :twitch.tv/tags\r\nPASS SCHMOOPIIE\r\nNICK {nick}\r\nJOIN #{channel}\r\n"
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    let mut silent = 0u32;
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Ok(()),
            Ok(_) => silent = 0,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // Nothing for 6 minutes means the connection silently died.
                silent += 1;
                if silent > 360 {
                    return Err(std::io::Error::other("no data from Twitch"));
                }
                continue;
            }
            Err(e) => return Err(e),
        }
        let msg = line.trim_end();
        if let Some(rest) = msg.strip_prefix("PING") {
            write!(stream, "PONG{rest}\r\n")?;
        } else if msg.contains(" JOIN #") || msg.contains(" 366 ") {
            *shared.state.lock().unwrap() = ChatState::Joined;
        } else if let Some(m) = parse_privmsg(msg) {
            let mut list = shared.messages.lock().unwrap();
            if list.len() >= KEEP {
                list.pop_front();
            }
            list.push_back(m);
            *shared.count.lock().unwrap() += 1;
        }
    }
}

/// Parses `@tags :nick!nick@host PRIVMSG #chan :text`.
pub fn parse_privmsg(line: &str) -> Option<ChatMessage> {
    let (tags, rest) = match line.strip_prefix('@') {
        Some(t) => t.split_once(' ')?,
        None => ("", line),
    };
    let rest = rest.strip_prefix(':')?;
    let (prefix, rest) = rest.split_once(' ')?;
    let rest = rest.strip_prefix("PRIVMSG ")?;
    let (_, text) = rest.split_once(" :")?;
    let login = prefix.split('!').next().unwrap_or(prefix);

    let mut name = login.to_string();
    let mut color = None;
    for tag in tags.split(';') {
        match tag.split_once('=') {
            Some(("display-name", v)) if !v.is_empty() => name = unescape(v),
            Some(("color", v)) => color = parse_color(v),
            _ => {}
        }
    }
    let (text, action) = match text
        .strip_prefix("\u{1}ACTION ")
        .and_then(|t| t.strip_suffix('\u{1}'))
    {
        Some(t) => (t, true),
        None => (text, false),
    };
    Some(ChatMessage {
        name,
        color,
        text: text.to_string(),
        action,
    })
}

fn unescape(v: &str) -> String {
    v.replace("\\s", " ")
        .replace("\\:", ";")
        .replace("\\\\", "\\")
}

fn parse_color(v: &str) -> Option<[u8; 3]> {
    let hex = v.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privmsg_with_tags() {
        let m = parse_privmsg(
            "@badge-info=;color=#1E90FF;display-name=Chimbita;mod=0 :chimbita!chimbita@chimbita.tmi.twitch.tv PRIVMSG #chimbita :hola :) a todos",
        )
        .unwrap();
        assert_eq!(m.name, "Chimbita");
        assert_eq!(m.color, Some([0x1e, 0x90, 0xff]));
        assert_eq!(m.text, "hola :) a todos");
        assert!(!m.action);
    }

    #[test]
    fn action_and_no_tags() {
        let m =
            parse_privmsg(":bob!bob@bob.tmi.twitch.tv PRIVMSG #x :\u{1}ACTION waves\u{1}").unwrap();
        assert_eq!(
            (m.name.as_str(), m.text.as_str(), m.action),
            ("bob", "waves", true)
        );
        assert_eq!(m.color, None);
    }

    #[test]
    fn other_lines_ignored() {
        assert!(parse_privmsg(":tmi.twitch.tv 001 justinfan1 :Welcome, GLHF!").is_none());
        assert!(parse_privmsg("PING :tmi.twitch.tv").is_none());
    }
}
