# OpenProfiler daemon with the web UI, built from the Linux release archives (see
# .github/workflows/release.yml). The image also carries the native agent and the remote bundle, so
# it works as an init container that copies /opt/openprofiler/libopenprofiler_agent.so into a
# shared volume for `-agentpath`.
#   docker run --rm -p 7878:7878 -e OPENPROFILER_TOKEN=... ghcr.io/matusso/openprofiler
# Local build: extract openprofiler-<tag>-linux-<arch>.tar.gz into dist/docker/linux-<arch>/, then
#   docker buildx build --platform linux/amd64,linux/arm64 .
FROM gcr.io/distroless/cc-debian12:nonroot
ARG TARGETOS
ARG TARGETARCH
ARG VERSION=dev
ARG SOURCE=https://github.com/matusso/openprofiler
LABEL org.opencontainers.image.title="openprofiler" \
      org.opencontainers.image.description="OpenProfiler daemon and web UI" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.source="${SOURCE}" \
      org.opencontainers.image.licenses="Apache-2.0 AND MPL-2.0"
COPY dist/docker/${TARGETOS}-${TARGETARCH}/ /opt/openprofiler/
EXPOSE 7878
ENTRYPOINT ["/opt/openprofiler/openprofiler"]
CMD ["daemon", "--web", "--bind", "0.0.0.0"]
