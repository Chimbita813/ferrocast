//! Twitch login with the device code flow: the app shows a short code, the
//! user approves it on twitch.tv, and the app receives a token. Works for a
//! "Public" Twitch app, so there is no client secret to hide in the exe.

use serde::Deserialize;

const SCOPES: &str = "chat:read chat:edit";
const AGENT: &str = "Chimcast";

/// Where the user registers the Twitch app that provides the client ID.
pub const CONSOLE_PAGE: &str = "https://dev.twitch.tv/console/apps";

#[derive(Clone, Debug, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Clone, Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    pub login: String,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
}

#[derive(Deserialize)]
struct ErrorReply {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct Validate {
    login: String,
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn post(url: &str, form: &str) -> Result<(u32, Vec<u8>), String> {
    crate::http::post_form_status(url, AGENT, "Accept: application/json\r\n", form)
        .map_err(|e| format!("couldn't reach Twitch: {}", e.message()))
}

fn error_message(body: &[u8]) -> String {
    serde_json::from_slice::<ErrorReply>(body)
        .map(|e| e.message)
        .unwrap_or_else(|_| String::from_utf8_lossy(body).into_owned())
}

/// Step 1: get a code for the user to approve.
pub fn start(client_id: &str) -> Result<DeviceCode, String> {
    let form = format!("client_id={}&scopes={}", enc(client_id.trim()), enc(SCOPES));
    let (status, body) = post("https://id.twitch.tv/oauth2/device", &form)?;
    if status != 200 {
        return Err(format!("Twitch said: {}", error_message(&body)));
    }
    serde_json::from_slice(&body).map_err(|e| format!("unexpected reply from Twitch: {e}"))
}

/// Step 2: wait (blocking) until the user approves or the code expires.
pub fn wait(client_id: &str, code: &DeviceCode) -> Result<Tokens, String> {
    let form = format!(
        "client_id={}&scopes={}&device_code={}&grant_type={}",
        enc(client_id.trim()),
        enc(SCOPES),
        enc(&code.device_code),
        enc("urn:ietf:params:oauth:grant-type:device_code")
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(code.expires_in);
    let pause = std::time::Duration::from_secs(code.interval.max(1));
    while std::time::Instant::now() < deadline {
        std::thread::sleep(pause);
        let (status, body) = post("https://id.twitch.tv/oauth2/token", &form)?;
        if status == 200 {
            return finish(&body);
        }
        let msg = error_message(&body);
        if !msg.contains("authorization_pending") && !msg.contains("slow_down") {
            return Err(format!("login failed: {msg}"));
        }
    }
    Err("the login code expired; try again".into())
}

/// Gets a fresh access token with the refresh token (tokens last a few hours).
pub fn refresh(client_id: &str, refresh_token: &str) -> Result<Tokens, String> {
    let form = format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}",
        enc(client_id.trim()),
        enc(refresh_token)
    );
    let (status, body) = post("https://id.twitch.tv/oauth2/token", &form)?;
    if status != 200 {
        return Err(format!("Twitch login expired: {}", error_message(&body)));
    }
    finish(&body)
}

fn finish(body: &[u8]) -> Result<Tokens, String> {
    let reply: TokenReply =
        serde_json::from_slice(body).map_err(|e| format!("unexpected reply from Twitch: {e}"))?;
    let (status, body) = crate::http::get_status(
        "https://id.twitch.tv/oauth2/validate",
        AGENT,
        &format!("Authorization: OAuth {}\r\n", reply.access_token),
    )
    .map_err(|e| format!("couldn't reach Twitch: {}", e.message()))?;
    if status != 200 {
        return Err(format!(
            "Twitch rejected the new token: {}",
            error_message(&body)
        ));
    }
    let v: Validate =
        serde_json::from_slice(&body).map_err(|e| format!("unexpected reply from Twitch: {e}"))?;
    Ok(Tokens {
        access: reply.access_token,
        refresh: reply.refresh_token,
        login: v.login,
    })
}
