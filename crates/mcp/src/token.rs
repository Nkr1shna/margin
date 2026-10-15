//! `margin-mcp token <mga_…>`, `margin-mcp token --forget` and `margin-mcp whoami`:
//! running on an agent token (`docs/l1-brief.md` D11). An agent token is minted in
//! Settings → Organisation → Agents; it reads and writes context only, within the
//! agent's ceiling. It is checked against `GET /api/me` before it is stored, stored in
//! the secret store under `<url>#agent`, and never printed back.

use anyhow::{anyhow, bail};
use margin_shared::{Me, OrgRole, PERSON_PREFIX};

use crate::client::Client;
use crate::config::{TokenKind, TokenStore, agent_key, clean_agent_token, stored_token};

/// Checks `raw` with the service and stores it as `base`'s agent token. A token the
/// service refuses is not stored. Returns the `Me` the service answered.
pub async fn store_agent_token(
    base: &str,
    raw: &str,
    store: &dyn TokenStore,
    say: &mut dyn FnMut(&str),
) -> anyhow::Result<Me> {
    let token = clean_agent_token(raw)?;
    let client = Client::new(base, Some(token.clone()))?;
    let me = match client.me().await {
        Ok(me) => me,
        Err(e) if e.status.is_none() => {
            bail!("couldn't check the agent token, so it wasn't stored: {e}")
        }
        Err(e) => bail!("Margin refused the agent token, so it wasn't stored: {e}"),
    };
    if me.agent.is_none() {
        bail!(
            "{} didn't answer as an agent (a service from before agent tokens?); the token wasn't stored",
            client.base()
        );
    }
    store.set(&agent_key(client.base()), &token)?;
    say(&format!(
        "Stored the agent token for {} in the secret store. Signed in as {}.",
        client.base(),
        describe(&me)
    ));
    if store.get(client.base())?.is_some() {
        say(
            "A device sign-in (`margin-mcp login`) is also stored for this service; the agent token is used first.",
        );
    }
    Ok(me)
}

/// Removes `base`'s agent token. Says whether there was one.
pub fn forget_agent_token(
    base: &str,
    store: &dyn TokenStore,
    say: &mut dyn FnMut(&str),
) -> anyhow::Result<bool> {
    let removed = store.delete(&agent_key(base))?;
    if !removed {
        say(&format!("No agent token was stored for {base}."));
        return Ok(false);
    }
    let then = if store.get(base)?.is_some() {
        "margin-mcp now uses the device sign-in (`margin-mcp login`)."
    } else {
        "margin-mcp is signed out of it."
    };
    say(&format!(
        "Removed the agent token for {base} from the secret store; {then} The token still works until it's revoked in Settings → Organisation → Agents."
    ));
    Ok(true)
}

/// `margin-mcp whoami`: who the stored token signs in as, from `GET /api/me`.
pub async fn whoami(base: &str, store: &dyn TokenStore) -> anyhow::Result<String> {
    let (token, kind) = stored_token(store, base)?.ok_or_else(|| {
        anyhow!(
            "not signed in to {base}: run `margin-mcp token <token>` with an agent token, or `margin-mcp login`"
        )
    })?;
    let client = Client::new(base, Some(token))?;
    let me = client.me().await?;
    let how = match kind {
        TokenKind::Agent => "the agent token",
        TokenKind::Device => "the device sign-in",
    };
    Ok(format!("{} ({} with {how})", describe(&me), client.base()))
}

/// One line for a person: "agent triage-bot of @krishna, may read: company,
/// company/engineering", or "@krishna (owner) on the device \"Claude Code (mac)\"".
pub fn describe(me: &Me) -> String {
    let handle = me
        .person
        .as_ref()
        .map_or(me.github_login.as_str(), |p| p.handle.as_str());
    if let Some(agent) = &me.agent {
        let own = format!("{PERSON_PREFIX}{}", agent.owner_id);
        let reads: Vec<String> = agent
            .ceiling
            .iter()
            .map(|p| {
                if *p == own {
                    format!("@{handle}'s private notes and chats")
                } else {
                    p.clone()
                }
            })
            .collect();
        let reads = if reads.is_empty() {
            "nothing".to_string()
        } else {
            reads.join(", ")
        };
        let disabled = if agent.disabled { " (disabled)" } else { "" };
        return format!(
            "agent {}{disabled} of @{handle}, may read: {reads}",
            agent.name
        );
    }
    let role = me.person.as_ref().map(|p| match p.org_role {
        OrgRole::Owner => "owner",
        OrgRole::Admin => "admin",
        OrgRole::Member => "member",
    });
    let who = match role {
        Some(role) => format!("@{handle} ({role})"),
        None => format!("@{handle}"),
    };
    match &me.device {
        Some(device) => format!("{who} on the device \"{}\"", device.name),
        None => who,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use margin_shared::{Agent, AgentKind, Device, Person, PersonSettings, PersonState};

    fn person(role: OrgRole) -> Person {
        Person {
            id: "p1".into(),
            github_login: "Krishna".into(),
            github_id: None,
            handle: "krishna".into(),
            name: "Krishna".into(),
            avatar_url: None,
            org_role: role,
            state: PersonState::Active,
            groups: vec![],
            settings: PersonSettings::default(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn whoami_names_the_agent_its_owner_and_what_it_may_read() {
        let me = Me {
            github_login: "Krishna".into(),
            device: None,
            person: Some(person(OrgRole::Owner)),
            agent: Some(Agent {
                id: "a1".into(),
                owner_id: "p1".into(),
                name: "triage-bot".into(),
                kind: AgentKind::Margin,
                ceiling: vec!["company".into(), "person/p1".into()],
                disabled: true,
                token: None,
                created_at: Utc::now(),
            }),
        };
        assert_eq!(
            describe(&me),
            "agent triage-bot (disabled) of @krishna, may read: company, @krishna's private notes and chats"
        );
    }

    #[test]
    fn whoami_names_the_person_on_a_device_and_survives_an_old_service() {
        let device = Device {
            id: "d1".into(),
            name: "Claude Code (mac)".into(),
            created_at: Utc::now(),
            last_seen_at: None,
            current: true,
            online: false,
            person_id: None,
        };
        let mut me = Me {
            github_login: "Krishna".into(),
            device: Some(device),
            person: Some(person(OrgRole::Member)),
            agent: None,
        };
        assert_eq!(
            describe(&me),
            "@krishna (member) on the device \"Claude Code (mac)\""
        );
        me.person = None;
        assert_eq!(
            describe(&me),
            "@Krishna on the device \"Claude Code (mac)\""
        );
    }
}
