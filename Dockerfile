# puku-controld: the control plane. The worker has its own image
# (Dockerfile.workerd, libkrun only); a Cloud Hypervisor worker runs on the
# host as a systemd unit (deploy/systemd/puku-workerd.service).

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

# Ubuntu 24.04 rather than Debian: its ceph-common is Ceph 19 (Squid), the
# release a cephadm cluster on Ubuntu 24.04 runs. controld uses `ceph` and
# `rbd` to fence a dead host off its disks, delete finished disks and back
# them up; without shared disks (PUKU_RBD_POOL unset) they are never called.
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl ceph-common \
 && rm -rf /var/lib/apt/lists/*
# Unprivileged: controld only needs the network, Postgres and Ceph's client.
# Group 10001 too, so a host can let it read the Ceph keyring with
# `chgrp 10001 ceph.client.puku.keyring && chmod 640 ...`.
#
# The data directories exist in the image, owned by puku, so a named volume
# mounted on /var/lib/puku starts out writable by uid 10001 (Docker copies
# the image's directory, ownership included, into an empty named volume).
RUN groupadd -r -g 10001 puku && useradd -r -u 10001 -g 10001 -m puku \
 && install -d -o puku -g puku /var/lib/puku /var/lib/puku/archive /var/lib/puku/backup-tmp
COPY --from=build /src/target/release/puku-controld /usr/local/bin/puku-controld
# Migrations are embedded in the binary by sqlx::migrate!, so nothing else
# needs to ship.
USER puku
EXPOSE 7770
ENV PUKU_LISTEN_ADDR=0.0.0.0:7770 \
    PUKU_ARCHIVE_DIR=/var/lib/puku/archive \
    PUKU_DISK_BACKUP_TMP=/var/lib/puku/backup-tmp
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s \
  CMD curl -fsS http://127.0.0.1:7770/health || exit 1
ENTRYPOINT ["/usr/local/bin/puku-controld"]
