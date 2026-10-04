# puku-controld: the control plane. Workers are NOT containerized — workerd
# needs /dev/kvm and the msb toolchain on the host, so it ships as a systemd
# unit (deploy/systemd/puku-workerd.service).

FROM rust:1-bookworm AS build
WORKDIR /src
# Dependency layer first: source edits shouldn't rebuild the whole tree.
COPY Cargo.toml Cargo.lock ./
COPY crates/puku-cloud-proto/Cargo.toml crates/puku-cloud-proto/
COPY crates/puku-controld/Cargo.toml crates/puku-controld/
COPY crates/puku-workerd/Cargo.toml crates/puku-workerd/
COPY crates/puku-cloud-cli/Cargo.toml crates/puku-cloud-cli/
RUN mkdir -p crates/puku-cloud-proto/src crates/puku-controld/src \
             crates/puku-workerd/src crates/puku-cloud-cli/src \
 && echo "" > crates/puku-cloud-proto/src/lib.rs \
 && echo "fn main() {}" > crates/puku-controld/src/main.rs \
 && echo "fn main() {}" > crates/puku-workerd/src/main.rs \
 && echo "fn main() {}" > crates/puku-cloud-cli/src/main.rs \
 && cargo build --release -p puku-controld 2>/dev/null || true

COPY . .
# sqlx runs through runtime `query_as`, not the macros, so no database and no
# `cargo sqlx prepare` is needed at build time.
#
# Touch EVERY workspace crate root, not just controld's. The layer above
# compiled stub sources to cache dependencies; COPY restores the real ones
# but does not necessarily give them newer mtimes, so cargo happily reuses
# the stub artifacts and the build fails on "unresolved import
# puku_cloud_proto::session".
RUN find crates -name lib.rs -o -name main.rs | xargs touch \
 && cargo build --release -p puku-controld

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
# Unprivileged: controld only needs the network and Postgres.
RUN useradd -r -u 10001 -m puku
COPY --from=build /src/target/release/puku-controld /usr/local/bin/puku-controld
# Migrations are embedded in the binary by sqlx::migrate!, so nothing else
# needs to ship.
USER puku
EXPOSE 7770
ENV PUKU_LISTEN_ADDR=0.0.0.0:7770
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD curl -fsS http://127.0.0.1:7770/health || exit 1
ENTRYPOINT ["/usr/local/bin/puku-controld"]
