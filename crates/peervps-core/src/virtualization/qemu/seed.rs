//! cloud-init "NoCloud" seed served over HTTP on loopback.
//!
//! QEMU's user-mode network makes the host's 127.0.0.1 reachable from the
//! guest as 10.0.2.2, and `-smbios type=1,serial=ds=nocloud;s=<url>` points
//! cloud-init at it. This works the same on every host OS and needs no ISO
//! tooling.

use std::net::SocketAddr;

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio::task::AbortHandle;

use crate::Result;

/// Address the guest uses to reach the host's loopback under QEMU user networking.
pub const HOST_FROM_GUEST: &str = "10.0.2.2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub instance_id: String,
    pub hostname: String,
    pub user: String,
    pub password: String,
    pub ssh_keys: Vec<String>,
}

impl Seed {
    pub fn meta_data(&self) -> String {
        format!("instance-id: {}\nlocal-hostname: {}\n", self.instance_id, self.hostname)
    }

    pub fn user_data(&self) -> String {
        // JSON strings are valid YAML scalars, which sidesteps quoting rules for keys.
        let q = |s: &str| serde_json::Value::String(s.to_owned()).to_string();
        let keys: Vec<String> = self.ssh_keys.iter().map(|k| q(k)).collect();
        format!(
            "#cloud-config\n\
             users:\n  - name: {user}\n    groups: [sudo]\n    shell: /bin/bash\n    sudo: \"ALL=(ALL) NOPASSWD:ALL\"\n    lock_passwd: false\n    ssh_authorized_keys: [{keys}]\n\
             chpasswd:\n  expire: false\n  users:\n    - {{name: {user}, password: {pw}, type: text}}\n\
             ssh_pwauth: true\n\
             growpart: {{mode: auto, devices: [\"/\"]}}\n",
            user = q(&self.user),
            pw = q(&self.password),
            keys = keys.join(", "),
        )
    }
}

/// Serve `seed` until the returned handle is aborted; returns the guest-facing URL.
pub async fn serve(seed: Seed) -> Result<(String, AbortHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let (meta, user) = (seed.meta_data(), seed.user_data());
    let app = Router::new()
        .route("/meta-data", get(move || async move { meta }))
        .route("/user-data", get(move || async move { user }))
        .route("/vendor-data", get(|| async { "" }));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{HOST_FROM_GUEST}:{}/", addr.port()), task.abort_handle()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Seed {
        Seed {
            instance_id: "vm-1".into(),
            hostname: "pv-1".into(),
            user: "peervps".into(),
            password: "s3cret".into(),
            ssh_keys: vec!["ssh-ed25519 AAAA test@host".into()],
        }
    }

    #[test]
    fn user_data_is_cloud_config() {
        let ud = seed().user_data();
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("name: \"peervps\""));
        assert!(ud.contains("ssh_authorized_keys: [\"ssh-ed25519 AAAA test@host\"]"));
        assert!(ud.contains("password: \"s3cret\""));
        assert!(seed().meta_data().contains("instance-id: vm-1"));
    }

    #[tokio::test]
    async fn serves_seed_files() {
        let (url, handle) = serve(seed()).await.expect("serve");
        let port = url.trim_end_matches('/').rsplit(':').next().expect("port").to_owned();
        assert!(url.starts_with("http://10.0.2.2:"));
        let http = reqwest::Client::builder().no_proxy().build().expect("client");
        let body = http
            .get(format!("http://127.0.0.1:{port}/user-data"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("text");
        assert!(body.starts_with("#cloud-config"));
        let meta = http
            .get(format!("http://127.0.0.1:{port}/meta-data"))
            .send()
            .await
            .expect("get")
            .text()
            .await
            .expect("text");
        assert!(meta.contains("local-hostname: pv-1"));
        handle.abort();
    }
}
