//! Image references: how yoink writes `repo` + `tag` for docker, and
//! which registry an image lives on. Pure text functions, shared by
//! config validation, the Docker API layer and the commands.

/// Build the canonical OCI image reference from `repo` + `tag`. Tags
/// that look like content digests (`sha256:<hex>`) get separated by
/// `@`; everything else by `:`. Used everywhere yoink hands an image
/// reference to docker (container create, pull, image-present check).
#[must_use]
pub fn image_reference(image: &str, tag: &str) -> String {
    if tag.starts_with("sha256:") {
        format!("{image}@{tag}")
    } else {
        format!("{image}:{tag}")
    }
}

/// Returns the registry hostname embedded in an image reference, or
/// `None` for bare names that resolve against Docker Hub library.
///
/// Per OCI spec: the substring before the first `/` is treated as a
/// hostname iff it contains `.`, `:`, or equals `localhost`. Anything
/// else is a Docker Hub image reference and has no embedded host.
///
/// Examples: `postgres → None`, `ghcr.io/u/a → Some("ghcr.io")`,
/// `localhost:5000/x → Some("localhost:5000")`,
/// `4db05qgnlk.registry.depot.dev/api → Some("4db05qgnlk.registry.depot.dev")`.
#[must_use]
pub fn image_registry_host(image: &str) -> Option<&str> {
    // No `/` means no host: the `:` in `caddy:2` starts a tag, not a port.
    let (head, _) = image.split_once('/')?;
    (head == "localhost" || head.contains('.') || head.contains(':')).then_some(head)
}

/// Registry names that all mean Docker Hub. An image with no embedded
/// host (`caddy:2`, `library/postgres`) also resolves there.
const DOCKER_HUB_HOSTS: [&str; 5] = [
    "docker.io",
    "index.docker.io",
    "registry-1.docker.io",
    "registry.hub.docker.com",
    "hub.docker.com",
];

/// Reduce a registry name to a comparable host: no scheme, no path,
/// no default HTTPS port, lowercase, and one spelling for Docker Hub.
pub(crate) fn canonical_registry_host(raw: &str) -> String {
    let raw = raw.trim();
    let no_scheme = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let host = no_scheme
        .split_once('/')
        .map_or(no_scheme, |(h, _)| h)
        .to_ascii_lowercase();
    let host = host.strip_suffix(":443").unwrap_or(&host);
    if DOCKER_HUB_HOSTS.contains(&host) {
        "docker.io".to_string()
    } else {
        host.to_string()
    }
}

/// True when `image` lives on the registry that `server` names.
///
/// `registry.server` credentials are valid for that one registry.
/// Sending them with a pull from another registry makes that registry
/// reject the pull with 401, also for a public image.
#[must_use]
pub fn image_on_registry(image: &str, server: &str) -> bool {
    let image_host = image_registry_host(image).unwrap_or("docker.io");
    canonical_registry_host(image_host) == canonical_registry_host(server)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Regression for #108: `registry.server: ghcr.io` credentials went
    /// out with the Docker Hub pull of `caddy:2`, which then failed 401.
    #[test]
    fn image_on_registry_matches_only_the_configured_registry() {
        assert!(image_on_registry("ghcr.io/owner/app", "ghcr.io"));
        assert!(image_on_registry("ghcr.io/owner/app", "https://GHCR.io/"));
        assert!(image_on_registry("localhost:5000/x", "localhost:5000"));
        assert!(!image_on_registry("caddy", "ghcr.io"));
        assert!(!image_on_registry("library/caddy", "ghcr.io"));
        assert!(!image_on_registry("quay.io/owner/app", "ghcr.io"));
        assert!(!image_on_registry("localhost:5000/x", "localhost:5001"));
        // The default HTTPS port is the same registry with or without `:443`.
        assert!(image_on_registry("ghcr.io/owner/app", "ghcr.io:443"));
        assert!(image_on_registry("ghcr.io:443/owner/app", " ghcr.io "));
    }

    #[test]
    fn image_on_registry_treats_docker_hub_names_as_one_registry() {
        for server in [
            "docker.io",
            "index.docker.io",
            "registry-1.docker.io",
            "https://index.docker.io/v1/",
            "registry.hub.docker.com",
            "hub.docker.com",
        ] {
            // A tag in the image name is not a registry port.
            assert!(image_on_registry("caddy:2", server), "{server}");
            assert!(image_on_registry("caddy", server), "{server}");
            assert!(image_on_registry("owner/app", server), "{server}");
            assert!(
                image_on_registry("docker.io/library/caddy", server),
                "{server}"
            );
            assert!(!image_on_registry("ghcr.io/owner/app", server), "{server}");
        }
    }

    #[test]
    fn image_reference_uses_colon_for_tags_at_for_digests() {
        assert_eq!(
            image_reference("registry.example.com/bt-api", "v1.2.3"),
            "registry.example.com/bt-api:v1.2.3",
        );
        assert_eq!(
            image_reference("registry.example.com/bt-api", "sha256:abc123"),
            "registry.example.com/bt-api@sha256:abc123",
        );
    }

    #[test]
    fn image_registry_host_classification() {
        assert_eq!(image_registry_host("postgres"), None);
        assert_eq!(image_registry_host("redis"), None);
        assert_eq!(image_registry_host("caddy:2"), None);
        // user/repo on Docker Hub: still no embedded host (no `.`/`:`/localhost).
        assert_eq!(image_registry_host("library/postgres"), None);
        assert_eq!(image_registry_host("ghcr.io/u/a"), Some("ghcr.io"));
        assert_eq!(
            image_registry_host("4db05qgnlk.registry.depot.dev/api"),
            Some("4db05qgnlk.registry.depot.dev"),
        );
        assert_eq!(image_registry_host("localhost/x"), Some("localhost"));
        assert_eq!(
            image_registry_host("localhost:5000/x"),
            Some("localhost:5000"),
        );
        assert_eq!(
            image_registry_host("docker.io/library/postgres"),
            Some("docker.io"),
        );
    }
}
