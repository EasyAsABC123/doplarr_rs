//! Optional, restart-independent forwarding of triage decisions to a private service.
use anyhow::{Context, bail};
use std::{sync::Arc, time::Duration};
use twilight_http::Client as Discord;
use twilight_model::{
    application::interaction::{Interaction, InteractionData},
    channel::message::MessageFlags,
    http::interaction::{InteractionResponse, InteractionResponseType},
};
use twilight_util::builder::InteractionResponseDataBuilder;

#[derive(Clone)]
pub struct Triage {
    url: String,
    token: String,
    approver: String,
    client: reqwest::Client,
}

fn parse_button(value: &str) -> Option<(&str, &str)> {
    let mut parts = value.split(':');
    if parts.next()? != "triage" {
        return None;
    }
    let decision = parts.next()?;
    let id = parts.next()?;
    if !matches!(decision, "approve" | "reject")
        || id.len() != 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || parts.next().is_some()
    {
        return None;
    }
    Some((decision, id))
}

impl Triage {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Ok(url) = std::env::var("TRIAGE_APPROVAL_URL") else {
            return Ok(None);
        };
        let parsed = reqwest::Url::parse(&url)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            bail!("invalid TRIAGE_APPROVAL_URL");
        }
        let token = std::fs::read_to_string(std::env::var("TRIAGE_APPROVAL_TOKEN_FILE")?)?
            .trim()
            .to_string();
        let approver = std::env::var("TRIAGE_APPROVER_ID")?;
        if token.len() < 32 || approver.is_empty() || !approver.bytes().all(|b| b.is_ascii_digit())
        {
            bail!("invalid triage credential or approver configuration");
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Some(Self {
            url,
            token,
            approver,
            client,
        }))
    }

    pub fn is_button(interaction: &Interaction) -> bool {
        matches!(&interaction.data, Some(InteractionData::MessageComponent(data))
            if data.custom_id.starts_with("triage:"))
    }

    pub async fn handle(
        &self,
        interaction: &Interaction,
        discord: Arc<Discord>,
    ) -> anyhow::Result<()> {
        let client = discord.interaction(interaction.application_id);
        client
            .create_response(
                interaction.id,
                &interaction.token,
                &InteractionResponse {
                    kind: InteractionResponseType::DeferredChannelMessageWithSource,
                    data: Some(
                        InteractionResponseDataBuilder::new()
                            .flags(MessageFlags::EPHEMERAL)
                            .build(),
                    ),
                },
            )
            .await?;
        let result = self.forward(interaction).await;
        // Never echo service bodies or errors containing URLs/credentials to Discord.
        let message = match result {
            Ok(true) => {
                "Decision recorded. The incident thread will receive the execution outcome."
            }
            Ok(false) => {
                "Decision rejected: unauthorized, expired, changed, or already decided. Review the latest proposal."
            }
            Err(_) => {
                "Approval service unavailable. Check the thread before retrying; no new approval is implied by this message."
            }
        };
        client
            .update_response(&interaction.token)
            .content(Some(message))
            .await?;
        Ok(())
    }

    async fn forward(&self, interaction: &Interaction) -> anyhow::Result<bool> {
        let Some(InteractionData::MessageComponent(data)) = &interaction.data else {
            return Ok(false);
        };
        let Some((decision, proposal)) = parse_button(&data.custom_id) else {
            return Ok(false);
        };
        let Some(user) = interaction.author_id() else {
            return Ok(false);
        };
        if user.to_string() != self.approver {
            return Ok(false);
        }
        let Some(guild) = interaction.guild_id else {
            return Ok(false);
        };
        let Some(channel) = &interaction.channel else {
            return Ok(false);
        };
        let Some(parent) = channel.parent_id else {
            return Ok(false);
        };
        let Some(message) = &interaction.message else {
            return Ok(false);
        };
        let body = serde_json::json!({"proposal":proposal,"decision":decision,"user":user.to_string(),
            "guild":guild.to_string(),"parent_channel":parent.to_string(),
            "thread":channel.id.to_string(),"message":message.id.to_string()});
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await
            .context("triage decision request failed")?;
        if response.status().is_server_error() {
            bail!("triage service unavailable");
        }
        Ok(response.status().is_success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_exact_triage_decisions_are_accepted() {
        let id = "abcdef0123456789abcdef0123456789";
        assert_eq!(
            parse_button(&format!("triage:approve:{id}")),
            Some(("approve", id))
        );
        assert_eq!(
            parse_button(&format!("triage:reject:{id}")),
            Some(("reject", id))
        );
        for bad in [
            format!("request:{id}"),
            format!("triage:exec:{id}"),
            format!("triage:approve:{id}:extra"),
            "triage:approve:garbage".to_string(),
        ] {
            assert!(parse_button(&bad).is_none());
        }
    }
}
