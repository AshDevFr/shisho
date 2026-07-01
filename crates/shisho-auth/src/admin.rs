//! Admin resolution. The model is deliberately minimal and additive:
//!
//! - if `auth.admin_emails` or `auth.admin_groups` is non-empty, a user is an
//!   admin when their email is in `admin_emails` (case-insensitive) OR their
//!   OIDC group memberships intersect `admin_groups`;
//! - when both are empty, the first provisioned user (earliest `created_at`) is
//!   the admin.
//!
//! Group memberships come from the user's OIDC group claim, refreshed each
//! login (see the OIDC flow), so changing `admin_groups` takes effect on the
//! user's next request without a re-login. The user-management UI reuses this.

use sea_orm::{DatabaseConnection, DbErr, EntityTrait, QueryOrder};

use shisho_db::entities::users;
use shisho_db::repos::users::decode_groups;

/// Whether `user` is an admin under the email and group allowlists.
pub async fn is_admin(
    db: &DatabaseConnection,
    admin_emails: &[String],
    admin_groups: &[String],
    user: &users::Model,
) -> Result<bool, DbErr> {
    if !admin_emails.is_empty() || !admin_groups.is_empty() {
        return Ok(email_allowed(admin_emails, user) || group_allowed(admin_groups, user));
    }
    is_first_user(db, &user.id).await
}

/// Whether the user's email is in the allowlist (case-insensitive).
fn email_allowed(admin_emails: &[String], user: &users::Model) -> bool {
    user.email.as_deref().is_some_and(|email| {
        admin_emails
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(email))
    })
}

/// Whether any of the user's stored groups is in the allowlist.
fn group_allowed(admin_groups: &[String], user: &users::Model) -> bool {
    if admin_groups.is_empty() {
        return false;
    }
    let groups = decode_groups(user);
    groups.iter().any(|g| admin_groups.contains(g))
}

/// Whether `user_id` is the first provisioned user (earliest `created_at`,
/// then `id` to break ties deterministically).
async fn is_first_user(db: &DatabaseConnection, user_id: &str) -> Result<bool, DbErr> {
    let first = users::Entity::find()
        .order_by_asc(users::Column::CreatedAt)
        .order_by_asc(users::Column::Id)
        .one(db)
        .await?;
    Ok(first.is_some_and(|f| f.id == user_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shisho_db::conn::connect_in_memory;
    use shisho_db::repos::users::{OidcIdentity, find_or_create};

    async fn provision(db: &DatabaseConnection, sub: &str, email: &str) -> users::Model {
        provision_with_groups(db, sub, email, &[]).await
    }

    async fn provision_with_groups(
        db: &DatabaseConnection,
        sub: &str,
        email: &str,
        groups: &[&str],
    ) -> users::Model {
        find_or_create(
            db,
            &OidcIdentity {
                provider: "mock".into(),
                subject: sub.into(),
                email: Some(email.into()),
                groups: groups.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn first_user_is_admin_when_no_allowlist() {
        let db = connect_in_memory().await;
        let first = provision(&db, "s1", "a@example.com").await;
        let second = provision(&db, "s2", "b@example.com").await;

        assert!(is_admin(&db, &[], &[], &first).await.unwrap());
        assert!(!is_admin(&db, &[], &[], &second).await.unwrap());
    }

    #[tokio::test]
    async fn admin_emails_override_first_user() {
        let db = connect_in_memory().await;
        let first = provision(&db, "s1", "a@example.com").await;
        let second = provision(&db, "s2", "b@example.com").await;

        let allow = vec!["B@Example.com".to_string()]; // case-insensitive
        assert!(
            !is_admin(&db, &allow, &[], &first).await.unwrap(),
            "first user is not the admin once an allowlist exists"
        );
        assert!(is_admin(&db, &allow, &[], &second).await.unwrap());
    }

    #[tokio::test]
    async fn admin_groups_grant_admin() {
        let db = connect_in_memory().await;
        let first = provision(&db, "s1", "a@example.com").await;
        let member = provision_with_groups(&db, "s2", "b@example.com", &["shisho-admins"]).await;

        let groups = vec!["shisho-admins".to_string()];
        // The group member is admin even though they are not the first user and
        // there is no email allowlist.
        assert!(is_admin(&db, &[], &groups, &member).await.unwrap());
        // The first user is no longer the implicit admin once an allowlist exists.
        assert!(!is_admin(&db, &[], &groups, &first).await.unwrap());
    }

    #[tokio::test]
    async fn email_and_group_allowlists_are_unioned() {
        let db = connect_in_memory().await;
        let by_email = provision(&db, "s1", "boss@example.com").await;
        let by_group = provision_with_groups(&db, "s2", "b@example.com", &["ops"]).await;
        let neither = provision_with_groups(&db, "s3", "c@example.com", &["users"]).await;

        let emails = vec!["boss@example.com".to_string()];
        let groups = vec!["ops".to_string()];

        assert!(is_admin(&db, &emails, &groups, &by_email).await.unwrap());
        assert!(is_admin(&db, &emails, &groups, &by_group).await.unwrap());
        assert!(!is_admin(&db, &emails, &groups, &neither).await.unwrap());
    }
}
