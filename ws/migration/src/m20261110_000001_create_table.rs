use sea_orm_migration::{prelude::*, schema::*};

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum Channel {
    Table,
    Id,
    Name,
    LatestUpdateId,
}

#[derive(DeriveIden)]
enum Alias {
    Table,
    Id,
    ChannelId,
    Name,
}

#[derive(DeriveIden)]
enum Update {
    Table,
    Id,
    ChannelId,
    PersistentFile,
    Timestamp,
}

const FK_ALIAS_CHANNEL: &str = "fk-alias-channel_id";
const FK_UPDATE_CHANNEL: &str = "fk-update-channel_id";
const FK_CHANNEL_LATEST_UPDATE: &str = "fk-channel-latest_update_id";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // channel and update reference each other. Create channel without its
        // foreign key first and add the key once update exists.
        manager
            .create_table(
                Table::create()
                    .table(Channel::Table)
                    .col(big_pk_auto(Channel::Id))
                    .col(string_uniq(Channel::Name))
                    // When channels are created, they don't have an update.
                    .col(big_integer_null(Channel::LatestUpdateId))
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Alias::Table)
                    .col(big_pk_auto(Alias::Id))
                    .col(big_integer(Alias::ChannelId))
                    .col(string(Alias::Name))
                    .foreign_key(
                        ForeignKey::create()
                            .name(FK_ALIAS_CHANNEL)
                            .from(Alias::Table, Alias::ChannelId)
                            .to(Channel::Table, Channel::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(Update::Table)
                    .col(big_pk_auto(Update::Id))
                    .col(big_integer(Update::ChannelId))
                    .col(string(Update::PersistentFile))
                    .col(timestamp_with_time_zone(Update::Timestamp))
                    .foreign_key(
                        ForeignKey::create()
                            .name(FK_UPDATE_CHANNEL)
                            .from(Update::Table, Update::ChannelId)
                            .to(Channel::Table, Channel::Id),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_foreign_key(
                ForeignKey::create()
                    .name(FK_CHANNEL_LATEST_UPDATE)
                    .from(Channel::Table, Channel::LatestUpdateId)
                    .to(Update::Table, Update::Id)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Break the cycle first, then drop in reverse dependency order.
        manager
            .drop_foreign_key(
                ForeignKey::drop()
                    .name(FK_CHANNEL_LATEST_UPDATE)
                    .table(Channel::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .drop_table(Table::drop().table(Update::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Alias::Table).to_owned())
            .await?;
        manager
            .drop_table(Table::drop().table(Channel::Table).to_owned())
            .await
    }
}
