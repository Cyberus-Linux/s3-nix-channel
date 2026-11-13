//! The legacy channel configuration that lived as JSON files in the bucket.
//!
//! This is only needed to import old deployments into the database.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use s3_nix_channel::persistent::Client;
use serde::Deserialize;
use tracing::{debug, error, info, warn};

/// The configuration that lives in the S3 bucket as /channels.json.
#[derive(Deserialize, Debug)]
struct PersistentChannelsConfig {
    /// The list of all channels we serve. Each channel needs a
    /// corresponding <channel>.json file for configuration in the
    /// bucket.
    channels: Vec<String>,
}

/// The configuration of a single channel.
#[derive(Deserialize, Debug)]
pub struct ChannelConfig {
    /// The latest element in the channel. If this is foo, users can download it as channel/foo.tar.gz.
    pub latest: Option<String>,

    /// The file extension of the files being served. If this is set to ".iso",
    /// the files have to have the form "some-file-name.iso". Multiple periods
    /// in the file_extension are allowed (e.g. ".tar.xz").
    ///
    /// Must include the starting period. Defaults to ".tar.xz" for backward
    /// compatibility.
    #[serde(default = "default_channel_file_extension")]
    pub file_extension: String,

    /// Alternative names for this channel. This can be used for
    /// transparent renames.
    #[serde(default)]
    pub aliases: Vec<String>,

    /// Previous tarballs in this channel.
    #[serde(default)]
    pub previous: Vec<String>,
}

/// Removes duplicate entries from a vector.
///
/// We keep the first unique entry. All duplicate ones are removed. This
/// function retains the order of the remaining elements.
fn remove_duplicates<T>(vec: &mut Vec<T>) -> bool
where
    T: Eq + Ord + Clone,
{
    let original_size = vec.len();
    let mut seen = BTreeSet::new();
    vec.retain(|item| seen.insert(item.clone()));

    original_size != vec.len()
}

impl ChannelConfig {
    /// Remove duplicates in `previous`, including entries that are the same as
    /// `latest`. This happened for old releases, because we didn't prevent
    /// re-uploading the same file.
    ///
    /// Returns true, if we removed entries.
    pub fn remove_previous_duplicates(&mut self) -> bool {
        let original_size = self.previous.len();

        remove_duplicates(&mut self.previous);
        if let Some(latest) = &self.latest {
            self.previous.retain(|file| file != latest);
        }

        original_size != self.previous.len()
    }
}

fn default_channel_file_extension() -> String {
    ".tar.xz".to_owned()
}

/// Load the configuration of all channels from the bucket.
///
/// Channels whose configuration can't be read are skipped with an error
/// message.
pub async fn load_channels_config(client: &Client) -> Result<BTreeMap<String, ChannelConfig>> {
    let mut channels = BTreeMap::new();

    if !client.file_exists("channels.json").await? {
        warn!("No channels.json in bucket: Starting with empty configuration!");
        return Ok(channels);
    }

    let persistent_config: PersistentChannelsConfig =
        serde_json::from_slice(&client.read_file("channels.json").await?)
            .context("Failed to deserialize channels.json")?;

    debug!("Loaded channel config: {persistent_config:?}");

    for channel_name in persistent_config.channels {
        let config_file = format!("{channel_name}.json");
        let channel_config = client
            .read_file(&config_file)
            .await
            .context("Failed to read channel config")
            .and_then(|bytes| {
                serde_json::from_slice::<ChannelConfig>(&bytes)
                    .context("Failed to deserialize channel configuration")
            });
        match channel_config {
            Ok(channel_config) => {
                info!(
                    "Channel {channel_name} points to: {}",
                    channel_config.latest.as_deref().unwrap_or("(nothing yet)")
                );

                channels.insert(channel_name, channel_config);
            }
            Err(err) => {
                error!("Failed reading configuration for {channel_name:?}, with path {config_file:?} in the bucket. Ignoring.");
                info!("Context: {}", err);
                info!("Cause: {}", err.root_cause());
                continue;
            }
        }
    }

    Ok(channels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_duplicates_works() {
        // Test with empty vec
        let mut vec: Vec<i32> = vec![];
        assert!(!remove_duplicates(&mut vec));
        assert_eq!(vec, Vec::<i32>::new());

        // Test with duplicates
        let mut vec = vec![1, 2, 3, 2, 4, 3, 5];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1, 2, 3, 4, 5]);

        // Test with no duplicates
        let mut vec = vec![1, 2, 3, 4, 5];
        assert!(!remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1, 2, 3, 4, 5]);

        // Test with single element
        let mut vec = vec![1];
        assert!(!remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1]);

        // Test with all duplicates
        let mut vec = vec![1, 1, 1, 1];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec![1]);

        // Test with strings
        let mut vec = vec![
            "a".to_string(),
            "b".to_string(),
            "a".to_string(),
            "c".to_string(),
        ];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec!["a".to_string(), "b".to_string(), "c".to_string()]);

        // Test that order is preserved
        let mut vec = vec![3, 1, 2, 1, 3, 2];
        assert!(remove_duplicates(&mut vec));
        assert_eq!(vec, vec![3, 1, 2]);
    }

    fn channel(latest: Option<&str>, previous: &[&str]) -> ChannelConfig {
        ChannelConfig {
            latest: latest.map(str::to_owned),
            file_extension: default_channel_file_extension(),
            aliases: vec![],
            previous: previous.iter().map(|&p| p.to_owned()).collect(),
        }
    }

    #[test]
    fn remove_previous_duplicates_works() {
        // Nothing to remove.
        let mut cfg = channel(Some("c"), &["a", "b"]);
        assert!(!cfg.remove_previous_duplicates());
        assert_eq!(cfg.previous, ["a", "b"]);

        // Duplicates within previous.
        let mut cfg = channel(Some("c"), &["a", "b", "a"]);
        assert!(cfg.remove_previous_duplicates());
        assert_eq!(cfg.previous, ["a", "b"]);

        // The latest release also shows up in previous.
        let mut cfg = channel(Some("b"), &["a", "b", "b"]);
        assert!(cfg.remove_previous_duplicates());
        assert_eq!(cfg.previous, ["a"]);

        // Without a latest release, only duplicates are removed.
        let mut cfg = channel(None, &["a", "a"]);
        assert!(cfg.remove_previous_duplicates());
        assert_eq!(cfg.previous, ["a"]);
    }
}
