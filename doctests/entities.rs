// Entities shared by the doctests, each of which includes this file in a
// hidden line.

mod bakery {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "bakery")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    #[sea_orm(has_many)]
    pub cakes: HasMany<super::cake::Entity>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

mod cake {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "cake")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub bakery_id: Option<i32>,
    #[sea_orm(belongs_to, from = "bakery_id", to = "id")]
    pub bakery: BelongsTo<Option<super::bakery::Entity>>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

mod account {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "account")]
  pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub name: String,
  }

  impl ActiveModelBehavior for ActiveModel {}
}
