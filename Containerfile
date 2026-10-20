ARG RUST_IMAGE=docker.io/library/rust:1.99.1-bookworm
ARG RUNTIME_IMAGE=docker.io/library/debian:bookworm-slim

FROM ${RUST_IMAGE} AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock config.example.toml ./
COPY src ./src
COPY assets ./assets
COPY sidecar/nonebot/catalog.json ./sidecar/nonebot/catalog.json
# A single compiler job also works on small deployment hosts.
ARG BUILD_JOBS=1
ARG CARGO_MIRROR=""
RUN --mount=type=cache,id=qkbot-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=qkbot-cargo-target,target=/build/target \
    if [ -n "${CARGO_MIRROR}" ]; then \
      cargo --config 'source.crates-io.replace-with="build-mirror"' \
        --config "source.build-mirror.registry=\"${CARGO_MIRROR}\"" \
        build --release --locked --jobs "${BUILD_JOBS}"; \
    else cargo build --release --locked --jobs "${BUILD_JOBS}"; fi \
    && install -D target/release/qkbot /out/qkbot

FROM ${RUNTIME_IMAGE} AS runtime
ARG DEBIAN_MIRROR=""
RUN if [ -n "${DEBIAN_MIRROR}" ]; then \
      sed -i "s|http://deb.debian.org|${DEBIAN_MIRROR}|g" /etc/apt/sources.list.d/debian.sources; \
    fi \
    && apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates fonts-noto-cjk tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 qkbot \
    && useradd --uid 10001 --gid 10001 --home-dir /data --no-create-home qkbot \
    && install -d -o 10001 -g 10001 -m 0700 /data /usr/share/qkbot
COPY --from=build /out/qkbot /usr/local/bin/qkbot
COPY config.example.toml /usr/share/qkbot/config.example.toml
COPY deploy/container-entrypoint.sh /usr/local/bin/qkbot-entrypoint
COPY assets/native/README.md assets/native/sources.json /usr/share/qkbot/sources/
COPY assets/native/wordle/LICENSE assets/native/wordle/LICENSE-pyspellchecker /usr/share/qkbot/sources/wordle/
COPY assets/native/kfc/LICENSE /usr/share/qkbot/sources/kfc/
COPY assets/native/meal/LICENSE /usr/share/qkbot/sources/meal/
COPY assets/native/meal/images/ /usr/share/qkbot/meal-images/
RUN chmod 0755 /usr/local/bin/qkbot-entrypoint
ARG VCS_REF=unknown
LABEL org.opencontainers.image.title="小鱼 qkbot-r" \
      org.opencontainers.image.source="https://github.com/Fox-yun/qkbot-r" \
      org.opencontainers.image.revision="${VCS_REF}"
ENV HOME=/data TZ=Asia/Shanghai QKBOT_MEAL_IMAGES_DIR=/usr/share/qkbot/meal-images
USER 10001:10001
WORKDIR /data
VOLUME ["/data"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/qkbot-entrypoint"]
