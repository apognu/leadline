//! Entities with every kind of relation, in SeaORM 2.0's dense format, where
//! relations are declared as fields and `#[sea_orm::model]` derives the
//! `Relation` enum and `Related` impls:
//!
//! - `bakery` has many `cake`s, many `baker`s and one `bakery_profile`;
//! - `cake` belongs to a `bakery`, and has many `filling`s through the
//!   `cake_filling` junction table (composite primary key);
//! - `baker` belongs to a `bakery` and, optionally, to a mentor `baker`
//!   (self-referential).

pub mod bakery {
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
    #[sea_orm(has_many)]
    pub bakers: HasMany<super::baker::Entity>,
    #[sea_orm(has_one)]
    pub profile: HasOne<super::bakery_profile::Entity>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

pub mod bakery_profile {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "bakery_profile")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    #[sea_orm(unique)]
    pub bakery_id: i32,
    pub description: String,
    #[sea_orm(belongs_to, from = "bakery_id", to = "id")]
    pub bakery: BelongsTo<super::bakery::Entity>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

pub mod cake {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "cake")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub price_cents: i32,
    pub bakery_id: Option<i32>,
    #[sea_orm(belongs_to, from = "bakery_id", to = "id")]
    pub bakery: BelongsTo<Option<super::bakery::Entity>>,
    #[sea_orm(has_many, via = "cake_filling")]
    pub fillings: HasMany<super::filling::Entity>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

pub mod filling {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "filling")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    #[sea_orm(has_many, via = "cake_filling")]
    pub cakes: HasMany<super::cake::Entity>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

pub mod cake_filling {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "cake_filling")]
  pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub cake_id: i32,
    #[sea_orm(primary_key, auto_increment = false)]
    pub filling_id: i32,
    #[sea_orm(belongs_to, from = "cake_id", to = "id")]
    pub cake: BelongsTo<super::cake::Entity>,
    #[sea_orm(belongs_to, from = "filling_id", to = "id")]
    pub filling: BelongsTo<super::filling::Entity>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

pub mod baker {
  use sea_orm::entity::prelude::*;

  #[sea_orm::model]
  #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
  #[sea_orm(table_name = "baker")]
  pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub name: String,
    pub bakery_id: i32,
    pub mentor_id: Option<i32>,
    #[sea_orm(belongs_to, from = "bakery_id", to = "id")]
    pub bakery: BelongsTo<super::bakery::Entity>,
    /// Self-referential: a baker's mentor is another baker.
    #[sea_orm(self_ref, relation_enum = "Mentor", from = "MentorId", to = "Id")]
    pub mentor: BelongsTo<Option<Entity>>,
  }

  impl ActiveModelBehavior for ActiveModel {}
}

pub fn bakery(id: i32, name: &str) -> bakery::Model {
  bakery::Model { id, name: name.into() }
}

pub fn cake(id: i32, name: &str, bakery_id: Option<i32>) -> cake::Model {
  cake::Model {
    id,
    name: name.into(),
    price_cents: 1000 + id,
    bakery_id,
  }
}

pub fn filling(id: i32, name: &str) -> filling::Model {
  filling::Model { id, name: name.into() }
}

pub fn baker(id: i32, name: &str, mentor_id: Option<i32>) -> baker::Model {
  baker::Model {
    id,
    name: name.into(),
    bakery_id: 1,
    mentor_id,
  }
}
