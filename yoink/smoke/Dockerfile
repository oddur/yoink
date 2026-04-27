# Pinned to linux/amd64 so the smoke test works from any operator
# (incl. macOS arm64) — backtrack-eu-1 is x86_64. Without this pin,
# the operator's native arch sneaks into the image and the container
# crashes with "Exec format error" on the host.
FROM --platform=linux/amd64 alpine:3.19
# Tiny smoke-test image. The CMD is `sleep` long enough that yoink's
# health-probe sees a running container; short enough that a forgotten
# instance reaps itself within an hour.
CMD ["sleep", "3600"]
