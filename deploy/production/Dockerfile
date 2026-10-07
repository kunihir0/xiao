# syntax=docker/dockerfile:1.10
# Build and runtime bases are immutable multi-architecture manifest digests.
ARG BUILD_IMAGE=rust:1.99.0-bookworm@sha256:114c7a4425406451c2866b6aafe69fe29b1b298832db1277d411ac73c82d04d6
ARG RUNTIME_IMAGE=debian:bookworm-20250929-slim@sha256:7e490910eea2861b9664577a96b54ce68ea3e02ce7f51d89cb0103a6f9c386e0
ARG DEBIAN_SNAPSHOT=20261005T000000Z

FROM ${BUILD_IMAGE} AS build
ARG DEBIAN_SNAPSHOT
WORKDIR /src

RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      'Suites: bookworm bookworm-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      > /etc/apt/sources.list.d/snapshot.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      'Suites: bookworm-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      >> /etc/apt/sources.list.d/snapshot.sources \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install --no-install-recommends --yes \
      ca-certificates=20250419~deb12u1 \
      libpq-dev=15.19-0+deb12u1 \
      pkg-config=1.8.1-1 \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY apps ./apps
COPY crates ./crates
COPY migrations ./migrations

RUN cargo build --locked --release --package bot

FROM ${RUNTIME_IMAGE} AS runtime
ARG DEBIAN_SNAPSHOT

RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/" \
      'Suites: bookworm bookworm-updates' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      > /etc/apt/sources.list.d/snapshot.sources \
    && printf '%s\n' \
      'Types: deb' \
      "URIs: https://snapshot.debian.org/archive/debian-security/${DEBIAN_SNAPSHOT}/" \
      'Suites: bookworm-security' \
      'Components: main' \
      'Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg' \
      >> /etc/apt/sources.list.d/snapshot.sources \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install --no-install-recommends --yes \
      ca-certificates=20250419~deb12u1 \
      libpq5=15.19-0+deb12u1 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 xiao \
    && useradd --system --uid 10001 --gid xiao --no-create-home --shell /usr/sbin/nologin xiao

COPY --from=build /src/target/release/bot /usr/local/bin/xiao-bot
COPY deploy/production/entrypoint.sh /usr/local/bin/xiao-entrypoint
RUN chmod 0555 /usr/local/bin/xiao-entrypoint

USER 10001:10001
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/xiao-entrypoint"]
