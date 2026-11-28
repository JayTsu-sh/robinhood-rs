# Build the console once for the same image that ships the daemon.
FROM node:22-bookworm-slim AS console
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/config config
COPY web/src src
COPY web/tsconfig.json ./
RUN npm run build

# Debian trixie can load the Ubuntu 24.04 Lustre userspace libraries.
FROM rust:trixie AS build
WORKDIR /src

RUN apt-get update && apt-get install -y --no-install-recommends \
        libclang-dev libssl-dev pkg-config libnl-genl-3-200 libreadline8t64 libyaml-0-2 \
    && rm -rf /var/lib/apt/lists/*

# Whamcloud ships the UAPI headers separately in the client modules package.
# Extract only its headers. Neither stage installs any Lustre kernel modules.
ADD --checksum=sha256:1c03caac981736f25440968269e7f324ab1851ac1fa910e3beb0df3ff38b790e https://downloads.whamcloud.com/public/lustre/lustre-2.16.1/ubuntu2404/client/lustre-client-utils_2.16.1-1_amd64.deb /tmp/lustre-client-utils.deb
ADD --checksum=sha256:aabbf9c9bfdc1505f3b630c10f565f8e35832cee95fba341330fc96bd01b286a https://downloads.whamcloud.com/public/lustre/lustre-2.16.1/ubuntu2404/client/lustre-dev_2.16.1-1_amd64.deb /tmp/lustre-dev.deb
ADD --checksum=sha256:2c851e0107d7e38f63dd0158d16a974e557959be77ed436e48af7589b497e5a9 https://downloads.whamcloud.com/public/lustre/lustre-2.16.1/ubuntu2404/client/lustre-client-modules-6.8.0-31-generic_2.16.1-1_amd64.deb /tmp/lustre-headers.deb
RUN dpkg-deb -x /tmp/lustre-client-utils.deb / \
    && dpkg-deb -x /tmp/lustre-dev.deb / \
    && dpkg-deb --fsys-tarfile /tmp/lustre-headers.deb | tar -x -C / ./usr/include \
    && ldconfig

# Cargo.lock pins this public git dependency. HTTPS works without SSH credentials.
RUN git config --system url."https://github.com/JayTsu-sh/scheduler-rs.git".insteadOf \
        "ssh://git@github.com/JayTsu-sh/scheduler-rs.git" \
    && git config --system http."https://github.com/JayTsu-sh/scheduler-rs.git".version HTTP/1.1

COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY src src
RUN CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --locked --release -p robinhood-rs -p rbh-cli

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3t64 libnl-genl-3-200 libreadline8t64 libyaml-0-2 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /tmp/fake-mount
COPY --from=build /src/target/release/robinhood /usr/sbin/robinhood
COPY --from=build /src/target/release/rbh /usr/bin/rbh
COPY --from=build /src/crates/rbh-entry-store/schema.sql /usr/share/robinhood/schema.sql
COPY --from=console /web/dist/ /usr/share/robinhood/console/
COPY --from=build /usr/lib/liblustreapi.so.1.0.0 /usr/local/lib/liblustreapi.so.1
COPY --from=build /usr/lib/liblnetconfig.so.4.0.0 /usr/local/lib/liblnetconfig.so.4
RUN ldconfig

EXPOSE 8080
ENTRYPOINT ["/usr/sbin/robinhood"]
