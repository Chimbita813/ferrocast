//! Streamlabs test alerts: the same "Test Follow / Test Bits" buttons Streamlabs
//! Desktop has. Streamlabs sends the test to every alert box widget on the
//! account, so the browser sources showing them play it.

/// (button label, API type) for Twitch.
pub const TEST_ALERTS: &[(&str, &str)] = &[
    ("Follow", "follow"),
    ("Subscription", "subscription"),
    ("Bits", "bits"),
    ("Donation", "donation"),
    ("Raid", "raid"),
    ("Host", "host"),
];

/// Where the user finds their API access token.
pub const TOKEN_PAGE: &str = "https://streamlabs.com/dashboard#/settings/api-settings";

/// Asks Streamlabs to play a test alert of `kind` (blocking; call off the UI thread).
pub fn send_test_alert(token: &str, kind: &str) -> Result<(), String> {
    let token = token.trim();
    if token.is_empty() {
        return Err("no Streamlabs API token".into());
    }
    let form = format!(
        "type={kind}&platform=twitch&access_token={}",
        form_encode(token)
    );
    crate::http::post_form(
        "https://streamlabs.com/api/v2.0/alerts/send_test_alert",
        "Chimcast",
        &format!("Authorization: Bearer {token}\r\nAccept: application/json\r\n"),
        &form,
    )
    .map(|_| ())
    .map_err(|e| {
        let msg = e.message();
        if msg.contains("HTTP 401") || msg.contains("HTTP 403") {
            "Streamlabs rejected the token; copy it again from API Settings".into()
        } else {
            msg
        }
    })
}

fn form_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
