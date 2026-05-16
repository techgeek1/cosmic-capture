//! Desktop notifications via `org.freedesktop.Notifications`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use zbus::{proxy, zvariant::Value, Connection};

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, &Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub async fn saved(path: &Path, kind_label: &str) -> Result<()> {
    let conn = Connection::session().await?;
    let proxy = NotificationsProxy::new(&conn).await?;
    let body = path.display().to_string();
    let mut hints = HashMap::new();
    let transient = Value::Bool(true);
    hints.insert("transient", &transient);
    proxy
        .notify(
            "cosmic-capture",
            0,
            "camera-photo-symbolic",
            &format!("{kind_label} saved"),
            &body,
            &[],
            hints,
            5000,
        )
        .await?;
    Ok(())
}
