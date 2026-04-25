//! Idempotent docker network setup. Wraps `DockerOps::ensure_network`
//! so callers don't need to think about it.

use crate::docker_ops::{DockerError, DockerOps, Host};

pub async fn ensure(ops: &dyn DockerOps, host: &Host, network: &str) -> Result<bool, DockerError> {
    ops.ensure_network(host, network).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docker_ops::{FakeDockerOps, RecordedCall};

    fn host() -> Host {
        Host {
            user: "deploy".into(),
            address: "host-a".into(),
        }
    }

    #[tokio::test]
    async fn ensure_propagates_already_exists() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(false));
        let created = ensure(&ops, &host(), "yoink").await.unwrap();
        assert!(!created);
        assert_eq!(
            ops.calls(),
            vec![RecordedCall::EnsureNetwork(host(), "yoink".into())]
        );
    }

    #[tokio::test]
    async fn ensure_propagates_created() {
        let ops = FakeDockerOps::new();
        ops.push_ensure_network(Ok(true));
        let created = ensure(&ops, &host(), "yoink").await.unwrap();
        assert!(created);
    }

    #[tokio::test]
    async fn ensure_propagates_error() {
        let ops = FakeDockerOps::new();
        let err = ensure(&ops, &host(), "yoink").await.unwrap_err();
        assert!(matches!(err, DockerError::FakeExhausted(_)));
    }
}
