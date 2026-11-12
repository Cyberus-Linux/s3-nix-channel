use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use migration::MigratorTrait;
use s3_nix_channel::persistent::Client;
use sea_orm::{Database, IntoActiveModel};
use tracing::warn;

#[derive(Subcommand, Debug)]
enum Commands {
    /// Migrate from bucket-based config to SQL database
    ImportLegacy {
        /// The S3 bucket to fetch the config from.
        bucket: String,

        /// The database URL.
        // TODO Explain the layout.
        db: String,

        #[arg(long, default_value_t = true)]
        apply_migrations: bool,
    },
    /// List all channels.
    ListChannels {
        /// The S3 bucket to upload the content to.
        bucket: String,
    },
    /// Add a channel.
    AddChannel {
        /// The S3 bucket to upload the content to.
        bucket: String,

        /// The channel to publish for.
        channel: String,

        /// The file extension for the channel.
        extension: String,
    },
    /// Add a new alias to a channel.
    AddAlias {
        /// The S3 bucket to upload the content to.
        bucket: String,

        /// The channel to add an alias to.
        channel: String,

        /// The new alias.
        alias: String,
    },
    /// Show the channel details.
    ShowChannel {
        /// The S3 bucket to upload the content to.
        bucket: String,

        /// The channel to publish for.
        channel: String,
    },
    Publish {
        /// The S3 bucket to upload the content to.
        bucket: String,

        /// The channel to publish for.
        channel: String,

        /// The file to upload.
        file: PathBuf,
    },
}

/// A program to serve a S3 bucket via the Nix Lockable Tarball Protocol.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    commands: Commands,
}

impl Args {
    fn bucket(&self) -> &str {
        match &self.commands {
            Commands::ImportLegacy {
                db: _,
                apply_migrations: _,
                bucket,
            } => bucket,
            Commands::ListChannels { bucket }
            | Commands::AddChannel {
                bucket,
                channel: _,
                extension: _,
            }
            | Commands::ShowChannel { bucket, channel: _ }
            | Commands::Publish {
                bucket,
                channel: _,
                file: _,
            } => bucket,
            Commands::AddAlias {
                bucket,
                channel: _,
                alias: _,
            } => bucket,
        }
    }
}

async fn import_legacy(s3_client: &Client, db: &str, apply_migrations: bool) -> Result<()> {
    use entity::{alias, channel, update};
    use sea_orm::prelude::*;

    let config = s3_client.load_channels_config().await?;

    let db = &Database::connect(db).await?;

    if apply_migrations {
        migration::Migrator::up(db, None).await?;
    } else {
        ensure!(
            migration::Migrator::get_pending_migrations_read_only(db)
                .await?
                .is_empty(),
            "Database has pending migrations. Aborting."
        );
    }

    for (channel_name, cfg) in config.channels() {
        let ext = &cfg.file_extension;
        let name = format!("{channel_name}{ext}");

        if alias::Entity::find_by_name(&name).one(db).await?.is_some() {
            warn!("Channel {name} already exists. Won't migrate legacy data.");
            continue;
        }

        db.transaction_async(async move |tx| -> Result<()> {
            // Inserts the channel together with its aliases.
            let channel = std::iter::once(name)
                .chain(cfg.aliases.iter().map(|a| format!("{a}{ext}")))
                .fold(channel::ActiveModel::builder(), |channel, alias| {
                    channel.add_alias(alias::ActiveModel::builder().set_name(alias))
                })
                .insert(tx)
                .await?;

            let add_update = |file: &str| {
                update::ActiveModel::builder()
                    .set_channel(channel.clone())
                    .set_persistent_file(format!("{file}{ext}"))
                    .set_timestamp(ChronoUtc::now())
                    .insert(tx)
            };

            for file in &cfg.previous {
                add_update(file).await?;
            }

            // The channel and its updates reference each other, so the latest
            // update can only be linked after both exist.
            if let Some(latest) = &cfg.latest {
                let latest = add_update(latest).await?;

                channel
                    .into_active_model()
                    .set_latest_update_id(Some(latest.id))
                    .update(tx)
                    .await?;
            }

            Ok(())
        })
        .await?;
    }

    Ok(())
}

async fn list_channels(s3_client: &Client) -> Result<()> {
    let config = s3_client.load_channels_config().await?;

    config
        .channels()
        .for_each(|(name, cfg)| println!("{name} ({})", cfg.file_extension));

    Ok(())
}

async fn add_channel(s3_client: &Client, channel: &str, extension: &str) -> Result<()> {
    s3_client
        .add_channel(channel, extension)
        .await
        .context("Failed to create channel")?;

    Ok(())
}

async fn add_alias(s3_client: &Client, channel: &str, alias: &str) -> Result<()> {
    s3_client
        .add_alias(channel, alias)
        .await
        .context("Failed to add alias")?;

    Ok(())
}

async fn show_channel(s3_client: &Client, channel: &str) -> Result<()> {
    let config = s3_client.load_channels_config().await?;

    println!(
        "Latest: {}",
        config
            .channel(channel, true)
            .context("No such channel")?
            .latest
            .as_deref()
            .unwrap_or("(nothing yet)")
    );

    Ok(())
}

async fn publish(s3_client: &Client, channel: &str, file: &Path) -> Result<()> {
    s3_client
        .update_channel(channel, file)
        .await
        .context("Failed to update channel")?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let s3_client = Client::new_from_env(args.bucket()).await?;

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    match args.commands {
        Commands::ImportLegacy {
            db,
            bucket: _,
            apply_migrations,
        } => import_legacy(&s3_client, &db, apply_migrations).await?,
        Commands::ListChannels { bucket: _ } => list_channels(&s3_client).await?,
        Commands::ShowChannel { bucket: _, channel } => show_channel(&s3_client, &channel).await?,
        Commands::AddChannel {
            bucket: _,
            channel,
            extension,
        } => add_channel(&s3_client, &channel, &extension).await?,
        Commands::AddAlias {
            bucket: _,
            channel,
            alias,
        } => add_alias(&s3_client, &channel, &alias).await?,
        Commands::Publish {
            bucket: _,
            channel,
            file,
        } => publish(&s3_client, &channel, &file).await?,
    }

    Ok(())
}
