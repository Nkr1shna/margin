//! `margin-mcp login`: the device sign-in every Margin app uses (`POST /auth/start`,
//! GitHub in the browser, `POST /auth/poll` every 2 s). The minted token is stored with
//! the `TokenStore` (the Keychain in the binary); the device shows in Settings → Devices
//! as "Claude Code (<host>)" and is revoked there like any other.

use std::time::Duration;

use anyhow::bail;
use chrono::Utc;
use margin_shared::LoginState;

use crate::client::Client;
use crate::config::TokenStore;

/// How often the service is asked whether the sign-in was approved.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// "Claude Code (<host>)", the device's name in Settings → Devices.
pub fn device_name() -> String {
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let host = host.trim_end_matches(".local");
    if host.is_empty() {
        "Claude Code".into()
    } else {
        format!("Claude Code ({host})")
    }
}

/// Signs in to `client`'s service and stores the token. `open` shows the GitHub page
/// (the system browser in the binary); `say` prints progress for the person.
pub async fn login(
    client: &Client,
    device_name: &str,
    store: &dyn TokenStore,
    open: impl FnOnce(&str) -> anyhow::Result<()>,
    say: &mut dyn FnMut(&str),
    interval: Duration,
) -> anyhow::Result<String> {
    let start = client.start_login(device_name).await?;
    say(&format!(
        "Sign in to Margin with GitHub to let Claude Code use it as \"{device_name}\":\n  {}",
        start.authorize_url
    ));
    if let Err(e) = open(&start.authorize_url) {
        say(&format!(
            "(Couldn't open the browser: {e}. Open the link above by hand.)"
        ));
    }
    loop {
        match client.poll_login(&start).await? {
            LoginState::Pending => {}
            LoginState::Approved {
                token,
                github_login,
            } => {
                store.set(client.base(), &token)?;
                say(&format!(
                    "Signed in to {} as {github_login}. The device \"{device_name}\" is in Settings → Devices; revoke it there to sign Claude Code out.",
                    client.base()
                ));
                return Ok(github_login);
            }
            LoginState::Denied { reason } => bail!("Margin refused the sign-in: {reason}"),
            LoginState::Expired => bail!("the sign-in expired; run `margin-mcp login` again"),
        }
        if Utc::now() > start.expires_at {
            bail!("the sign-in expired; run `margin-mcp login` again");
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_is_named_for_claude_code() {
        assert!(device_name().starts_with("Claude Code"));
    }
}
