//! Tenant scoping for the repository layer.
//!
//! Every repository method that reads or mutates tenant data takes a
//! [`UserScope`] and applies it internally, so a forgetful handler cannot leak
//! across users: the filter lives where the query is built, not in the caller.
//!
//! - [`UserScope::SingleUser`] applies no filter. It is the single-user-mode
//!   scope (one tenant; rows may carry a NULL `user_id`) and the admin/service
//!   scope (an API key with no owning user).
//! - [`UserScope::User`] confines every query to one `user_id`.
//!
//! A `UserScope` is derived from the authenticated identity (see the API
//! crate's auth context); the constructors here are the only way to make one,
//! and the intent is that they are reached from that derivation, not invented
//! ad hoc in a handler.

use sea_orm::{ColumnTrait, Condition};

/// The tenant a repository call is confined to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserScope {
    /// No row filter: single-user mode, or an unowned admin/service key.
    SingleUser,
    /// Confine to a single `user_id`.
    User(String),
}

impl UserScope {
    /// Scope to a specific user.
    pub fn user(id: impl Into<String>) -> Self {
        UserScope::User(id.into())
    }

    /// The `user_id` to stamp on rows this scope creates, if any. `SingleUser`
    /// stamps NULL (one tenant); `User` stamps that id.
    pub fn owner(&self) -> Option<String> {
        match self {
            UserScope::SingleUser => None,
            UserScope::User(id) => Some(id.clone()),
        }
    }

    /// A condition filtering `user_id_col` to this scope. Empty (matches all)
    /// for `SingleUser`; `user_id_col = ?` for `User`. Compose with `.add()`
    /// into a larger `Condition`.
    pub fn condition<C: ColumnTrait>(&self, user_id_col: C) -> Condition {
        match self {
            UserScope::SingleUser => Condition::all(),
            UserScope::User(id) => Condition::all().add(user_id_col.eq(id.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_user_stamps_no_owner() {
        assert_eq!(UserScope::SingleUser.owner(), None);
    }

    #[test]
    fn user_scope_stamps_its_id() {
        assert_eq!(UserScope::user("u1").owner(), Some("u1".to_string()));
    }
}
