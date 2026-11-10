pub mod channel {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "channel")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,

        #[sea_orm(unique)]
        pub name: String,

        #[sea_orm(has_many)]
        pub aliases: HasMany<super::alias::Entity>,

        // There might not be an update yet.
        pub latest_update_id: Option<i64>,

        #[sea_orm(belongs_to, from = "latest_update_id", to = "id")]
        pub latest_update: BelongsTo<Option<super::update::Entity>>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod alias {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "alias")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub channel_id: i64,
        pub alias: String,
        #[sea_orm(belongs_to, from = "channel_id", to = "id")]
        pub channel: BelongsTo<super::channel::Entity>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod update {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "update")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub channel_id: i64,
        pub persistent_file: String,

        pub timestamp: DateTimeUtc,

        #[sea_orm(belongs_to, from = "channel_id", to = "id")]
        pub channel: BelongsTo<super::channel::Entity>,
    }

    impl ActiveModelBehavior for ActiveModel {}
}
