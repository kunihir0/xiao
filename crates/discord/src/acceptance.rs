//! Live acceptance helper for inspecting and managing Discord commands via Serenity HTTP.
//!
//! Never prints, logs, or serializes the Discord token.

use poise::serenity_prelude as serenity;
use serenity::builder::CreateCommand;
use serenity::model::id::{CommandId, GuildId};

use crate::DiscordError;

/// Redacted identity report from authenticated Discord API check.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscordIdentityReport {
    pub application_id: u64,
    pub bot_username: String,
    pub bot_user_id: u64,
    pub guild_id: u64,
    pub guild_name: String,
}

/// Acceptance client that performs targeted, non-destructive Discord API checks.
pub struct DiscordAcceptanceClient {
    http: serenity::http::Http,
}

impl DiscordAcceptanceClient {
    /// Creates a new acceptance client. The token is never serialized or printed.
    pub fn new(token: impl AsRef<str>) -> Self {
        Self {
            http: serenity::http::Http::new(token.as_ref()),
        }
    }

    async fn ensure_application_id(&self) -> Result<(), DiscordError> {
        let app_info = self
            .http
            .get_current_application_info()
            .await
            .map_err(|err| DiscordError::Api(format!("failed to fetch application info: {err}")))?;
        self.http.set_application_id(app_info.id);
        Ok(())
    }

    /// Fetches all guilds the bot belongs to.
    pub async fn list_guilds(&self) -> Result<Vec<(u64, String)>, DiscordError> {
        let guilds = self.http.get_guilds(None, None).await.map_err(|err| {
            DiscordError::Api(format!("failed to fetch current user guilds: {err}"))
        })?;
        Ok(guilds.into_iter().map(|g| (g.id.get(), g.name)).collect())
    }

    /// Verifies that the token authenticates, discovers the bot application ID and username,
    /// and checks that the bot is a member of the approved guild.
    pub async fn verify_identity(
        &self,
        expected_app_id: Option<u64>,
        guild_id: u64,
    ) -> Result<DiscordIdentityReport, DiscordError> {
        let app_info = self
            .http
            .get_current_application_info()
            .await
            .map_err(|err| DiscordError::Api(format!("failed to fetch application info: {err}")))?;
        self.http.set_application_id(app_info.id);

        let current_user =
            self.http.get_current_user().await.map_err(|err| {
                DiscordError::Api(format!("failed to fetch current bot user: {err}"))
            })?;

        if let Some(expected) = expected_app_id
            && app_info.id.get() != expected
        {
            return Err(DiscordError::Api(format!(
                "authenticated application ID {} does not match expected {}",
                app_info.id.get(),
                expected
            )));
        }

        let guild = self
            .http
            .get_guild(GuildId::new(guild_id))
            .await
            .map_err(|err| DiscordError::Api(format!("failed to fetch guild {guild_id}: {err}")))?;

        Ok(DiscordIdentityReport {
            application_id: app_info.id.get(),
            bot_username: current_user.name.clone(),
            bot_user_id: current_user.id.get(),
            guild_id,
            guild_name: guild.name,
        })
    }

    /// Returns the number of global `/ping` commands registered for the application.
    pub async fn count_global_ping(&self) -> Result<usize, DiscordError> {
        self.ensure_application_id().await?;
        let commands =
            self.http.get_global_commands().await.map_err(|err| {
                DiscordError::Api(format!("failed to fetch global commands: {err}"))
            })?;
        Ok(commands.iter().filter(|c| c.name == "ping").count())
    }

    /// Returns the number of guild `/ping` commands registered in the specified guild.
    pub async fn count_guild_ping(&self, guild_id: u64) -> Result<usize, DiscordError> {
        self.ensure_application_id().await?;
        let commands = self
            .http
            .get_guild_commands(GuildId::new(guild_id))
            .await
            .map_err(|err| {
                DiscordError::Api(format!(
                    "failed to fetch guild commands for {guild_id}: {err}"
                ))
            })?;
        Ok(commands.iter().filter(|c| c.name == "ping").count())
    }

    /// Ensures that a `/ping` command exists in the specified guild without bulk overwrite.
    /// If `/ping` is already present, returns `(command_id, false)`.
    /// If `/ping` is not present, creates it via POST and returns `(command_id, true)`.
    pub async fn ensure_guild_ping(&self, guild_id: u64) -> Result<(u64, bool), DiscordError> {
        self.ensure_application_id().await?;
        let gid = GuildId::new(guild_id);
        let existing =
            self.http.get_guild_commands(gid).await.map_err(|err| {
                DiscordError::Api(format!("failed to fetch guild commands: {err}"))
            })?;

        if let Some(ping) = existing.iter().find(|c| c.name == "ping") {
            return Ok((ping.id.get(), false));
        }

        let create = CreateCommand::new("ping").description("Responds with pong");
        let created = self
            .http
            .create_guild_command(gid, &create)
            .await
            .map_err(|err| {
                DiscordError::Api(format!("failed to create guild ping command: {err}"))
            })?;

        Ok((created.id.get(), true))
    }

    /// Deletes a guild command by ID without touching other commands.
    pub async fn delete_guild_command(
        &self,
        guild_id: u64,
        command_id: u64,
    ) -> Result<(), DiscordError> {
        self.ensure_application_id().await?;
        self.http
            .delete_guild_command(GuildId::new(guild_id), CommandId::new(command_id))
            .await
            .map_err(|err| {
                DiscordError::Api(format!(
                    "failed to delete guild command {command_id}: {err}"
                ))
            })?;
        Ok(())
    }

    /// Verifies safe error path: requesting a nonexistent command returns an error.
    pub async fn verify_nonexistent_command_error(&self) -> Result<(), DiscordError> {
        let result = self.http.get_global_command(CommandId::new(1)).await;
        if result.is_err() {
            Ok(())
        } else {
            Err(DiscordError::Api(
                "querying nonexistent command unexpectedly succeeded".to_owned(),
            ))
        }
    }
}
