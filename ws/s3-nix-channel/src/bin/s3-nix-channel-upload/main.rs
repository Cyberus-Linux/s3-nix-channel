mod legacy;

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use entity::{alias, channel, update};
use migration::MigratorTrait;
use sea_orm::{
    prelude::*, ActiveValue::Set, Database, DatabaseConnection, IntoActiveModel, QueryOrder,
};
use storage::s3::Client;
use tracing::{info, warn};

#[derive(Subcommand, Debug)]
enum Commands {
    /// Migrate from bucket-based config to SQL database
    ImportLegacy {
        /// The S3 bucket to fetch the config from.
        bucket: String,

        #[arg(long, default_value_t = true)]
        apply_migrations: bool,
    },
    /// List all channels.
    ListChannels,
    /// Add a channel.
    AddChannel {
        /// The name of the new channel, including the file extension, for
        /// example: foobar-24.05.tar.xz
        channel: String,
    },
    /// Add a new alias to a channel.
    AddAlias {
        /// The channel to add an alias to.
        channel: String,

        /// The new alias.
        alias: String,

        /// Add the alias even if its extension doesn't match the channel.
        #[arg(long)]
        force: bool,
    },
    /// Show the channel details.
    ShowChannel {
        /// The channel to show.
        channel: String,
    },
    /// Upload a file and make it the latest update of a channel.
    Publish {
        /// The S3 bucket to upload the content to.
        bucket: String,

        /// The channel to publish for.
        channel: String,

        /// The file to upload.
        file: PathBuf,

        /// Publish even if the file extension doesn't match the channel.
        #[arg(long)]
        force: bool,
    },
}

/// A program to manage the channels served by s3-nix-channel.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Location of the database.
    ///
    /// Find more information about the format in the sea-orm manual:
    /// https://www.sea-ql.org/SeaORM/docs/install-and-config/connection/
    ///
    /// For postgres this can look like: postgres://username:password@host/database
    #[arg(long)]
    db: String,

    #[command(subcommand)]
    commands: Commands,
}

/// Connect to the database and make sure its schema is up-to-date.
async fn connect(db: &str, apply_migrations: bool) -> Result<DatabaseConnection> {
    let db = Database::connect(db)
        .await
        .context("Failed to connect to database")?;

    if apply_migrations {
        migration::Migrator::up(&db, None).await?;
    } else {
        ensure!(
            migration::Migrator::get_pending_migrations_read_only(&db)
                .await?
                .is_empty(),
            "Database has pending migrations. Aborting."
        );
    }

    Ok(db)
}

/// Look up a channel by any of its names.
async fn find_channel(db: &impl ConnectionTrait, name: &str) -> Result<channel::Model> {
    channel::Entity::find()
        .inner_join(alias::Entity)
        .filter(alias::Column::Name.eq(name))
        .one(db)
        .await?
        .with_context(|| format!("No such channel: {name}"))
}

/// File extensions we can recognize in channel and file names.
///
/// We need a list, because names like os-26.05.tar.xz contain periods outside
/// of the extension.
const KNOWN_EXTENSIONS: &[&str] = &[
    ".tar.xz", ".tar.gz", ".tar.zst", ".tar.bz2", ".tar", ".tgz", ".iso", ".zip",
];

/// Return the longest known extension that `name` ends with.
fn known_extension(name: &str) -> Option<&'static str> {
    KNOWN_EXTENSIONS
        .iter()
        .copied()
        .filter(|ext| name.ends_with(ext))
        .max_by_key(|ext| ext.len())
}

/// Check that `name` (a file or an alias) has the same known extension as
/// the channel. Returns the reason if it doesn't.
fn check_extension(channel_name: &str, name: &str) -> Result<(), String> {
    match (known_extension(channel_name), known_extension(name)) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (Some(expected), Some(actual)) => Err(format!(
            "{name} ends in {actual}, but channel {channel_name} serves {expected} files."
        )),
        (Some(expected), None) => Err(format!(
            "{name} doesn't end in {expected} like channel {channel_name}."
        )),
        (None, _) => Err(format!(
            "Can't determine the file type of channel {channel_name}. Known extensions are: {}",
            KNOWN_EXTENSIONS.join(", ")
        )),
    }
}

/// Fail if `name` doesn't match the channel's extension, unless `force` is set.
fn require_matching_extension(channel_name: &str, name: &str, force: bool) -> Result<()> {
    if let Err(reason) = check_extension(channel_name, name) {
        ensure!(force, "{reason}\nUse --force to skip this check.");
        warn!("{reason} Continuing anyway, because of --force.");
    }

    Ok(())
}

async fn import_legacy(s3_client: &Client, db: &DatabaseConnection) -> Result<()> {
    let config = legacy::load_channels_config(s3_client).await?;

    for (channel_name, mut cfg) in config {
        if cfg.remove_previous_duplicates() {
            info!("Skipping duplicate entries in the history of {channel_name}.");
        }

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

async fn list_channels(db: &DatabaseConnection) -> Result<()> {
    let channels = channel::Entity::find()
        .find_with_related(alias::Entity)
        .order_by_asc(channel::Column::Id)
        .order_by_asc(alias::Column::Id)
        .all(db)
        .await?;

    for (_, aliases) in channels {
        let names: Vec<&str> = aliases.iter().map(|a| a.name.as_str()).collect();
        println!("{}", names.join(", "));
    }

    Ok(())
}

async fn add_channel(db: &DatabaseConnection, name: &str) -> Result<()> {
    channel::ActiveModel::builder()
        .add_alias(alias::ActiveModel::builder().set_name(name))
        .insert(db)
        .await
        .context("Failed to create channel")?;

    Ok(())
}

async fn add_alias(db: &DatabaseConnection, channel: &str, alias: &str, force: bool) -> Result<()> {
    let channel_name = channel;
    let channel = find_channel(db, channel_name).await?;

    require_matching_extension(channel_name, alias, force)?;

    alias::ActiveModel::builder()
        .set_channel_id(channel.id)
        .set_name(alias)
        .insert(db)
        .await
        .context("Failed to add alias")?;

    Ok(())
}

async fn show_channel(db: &DatabaseConnection, name: &str) -> Result<()> {
    let channel = find_channel(db, name).await?;

    let latest = match channel.latest_update_id {
        Some(id) => update::Entity::find_by_id(id).one(db).await?,
        None => None,
    };

    println!(
        "Latest: {}",
        latest
            .as_ref()
            .map(|u| u.persistent_file.as_str())
            .unwrap_or("(nothing yet)")
    );

    Ok(())
}

async fn publish(
    db: &DatabaseConnection,
    s3_client: &Client,
    channel_name: &str,
    file: &Path,
    force: bool,
) -> Result<()> {
    let channel = find_channel(db, channel_name).await?;

    let file_name = file
        .file_name()
        .with_context(|| format!("No file name: {}", file.display()))?
        .to_string_lossy();

    require_matching_extension(channel_name, &file_name, force)?;

    let object_key = s3_client.upload_new_file(file).await?;

    // Record the update and make it the latest in one go.
    let persistent_file = object_key.clone();
    db.transaction_async(async move |tx| -> Result<()> {
        let update = update::ActiveModel::builder()
            .set_channel_id(channel.id)
            .set_persistent_file(persistent_file)
            .set_timestamp(ChronoUtc::now())
            .insert(tx)
            .await?;

        let mut channel = channel.into_active_model();
        channel.latest_update_id = Set(Some(update.id));
        channel.update(tx).await?;

        Ok(())
    })
    .await
    .with_context(|| {
        format!("Failed to update channel. This leaked {object_key}! Remove it manually, if this is an issue.")
    })?;

    println!("Channel {channel_name} now points to {object_key}.");

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Only the import is allowed to change the schema.
    let apply_migrations = matches!(
        args.commands,
        Commands::ImportLegacy {
            apply_migrations: true,
            ..
        }
    );
    let db = &connect(&args.db, apply_migrations).await?;

    match args.commands {
        Commands::ImportLegacy { bucket, .. } => {
            let s3_client = Client::new_from_env(&bucket).await?;
            import_legacy(&s3_client, db).await?
        }
        Commands::ListChannels => list_channels(db).await?,
        Commands::ShowChannel { channel } => show_channel(db, &channel).await?,
        Commands::AddChannel { channel } => add_channel(db, &channel).await?,
        Commands::AddAlias {
            channel,
            alias,
            force,
        } => add_alias(db, &channel, &alias, force).await?,
        Commands::Publish {
            bucket,
            channel,
            file,
            force,
        } => {
            let s3_client = Client::new_from_env(&bucket).await?;
            publish(db, &s3_client, &channel, &file, force).await?
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extension_ignores_periods_in_versions() {
        assert_eq!(known_extension("os-26.05.tar.xz"), Some(".tar.xz"));
        assert_eq!(known_extension("install-24.05.iso"), Some(".iso"));
        assert_eq!(known_extension("foo.tar"), Some(".tar"));
        assert_eq!(known_extension("os-26.05"), None);
        assert_eq!(known_extension("notes.txt"), None);
    }

    #[test]
    fn check_extension_works() {
        assert!(check_extension("os-26.05.tar.xz", "os-c220c710.tar.xz").is_ok());
        assert!(check_extension("os-26.05.tar.xz", "media-1234.iso").is_err());
        assert!(check_extension("os-26.05.tar.xz", "os-c220c710.tar").is_err());
        assert!(check_extension("os-26.05.tar.xz", "os-c220c710").is_err());
        assert!(check_extension("new-24.05.txt", "new.txt").is_err());

        // Aliases
        assert!(check_extension("os-26.05.tar.xz", "linux-26.05.tar.xz").is_ok());
        assert!(check_extension("os-26.05.tar.xz", "linux-26.05").is_err());
    }
}
