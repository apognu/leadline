//! A single entity without relations, for tests about the mock itself.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "cake")]
pub struct Model {
  #[sea_orm(primary_key)]
  pub id: i32,
  pub name: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}

/// A cake with this ID and name.
pub fn cake(id: i32, name: &str) -> Model {
  Model { id, name: name.into() }
}
