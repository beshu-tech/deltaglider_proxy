// SPDX-License-Identifier: GPL-3.0-only

mod crd;
mod reconcile;
mod resources;

use kube::CustomResourceExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("crd") {
        // `deltaglider-operator crd` prints the CRD YAML (source of deploy/crd.yaml).
        print!("{}", serde_yaml::to_string(&crd::DeltaGliderProxy::crd())?);
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let client = kube::Client::try_default().await?;
    tracing::info!("deltaglider-operator starting");
    reconcile::run(client).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crd_yaml_in_deploy_dir_is_current() {
        let generated = serde_yaml::to_string(&crd::DeltaGliderProxy::crd()).unwrap();
        let on_disk = include_str!("../deploy/crd.yaml");
        assert_eq!(
            generated, on_disk,
            "deploy/crd.yaml is stale — regenerate with `cargo run -- crd > deploy/crd.yaml`"
        );
    }
}
