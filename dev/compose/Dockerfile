# Multi-stage build for the robinhood-rs daemon.
#
# The daemon links against liblustreapi via FFI; the dev image bundles
# the Lustre client libs from rockylinux to satisfy the dynamic linker
# even when no MDT is reachable. The listener is off by default in the
# compose stack so no Lustre mount is required at runtime.

FROM rust:1.82-bookworm AS build
WORKDIR /src

# Build deps. lustre-client is Red Hat only; on Debian we fake
# liblustreapi with a stub so the dev container links. The stub panics
# if called — which is fine because RBH_CHANGELOG_USER is unset in the
# compose stack (the FS-scan path uses different ffi).
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev clang llvm-dev libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# Stub liblustreapi so bindgen + linker are happy.
RUN mkdir -p /usr/local/include/lustre && \
    echo 'int llapi_fid2path_at(){return -1;}\nint llapi_get_fsname(){return -1;}\nint llapi_fd2fid(){return -1;}\nint llapi_path2fid(){return -1;}\nint llapi_search_mounts(){return -1;}\nint llapi_obd_statfs(){return -1;}\nint llapi_get_obd_count(){return -1;}\nint llapi_getstripe(){return -1;}\nint llapi_hsm_state_get(){return -1;}\nint llapi_hsm_state_set(){return -1;}\nint llapi_hsm_request(){return -1;}\nint llapi_changelog_start(){return -1;}\nint llapi_changelog_fini(){return -1;}\nint llapi_changelog_recv(){return -1;}\nint llapi_changelog_free(){return -1;}\nint llapi_changelog_clear(){return -1;}\n' > /tmp/stub.c && \
    gcc -shared -fPIC -o /usr/local/lib/liblustreapi.so /tmp/stub.c && \
    ln -s liblustreapi.so /usr/local/lib/liblustreapi.so.1 && \
    ldconfig

COPY . /src
RUN cargo build --release -p robinhood-rs -p rbh-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/robinhood /usr/sbin/robinhood
COPY --from=build /src/target/release/rbh /usr/bin/rbh
COPY --from=build /usr/local/lib/liblustreapi.so /usr/local/lib/liblustreapi.so
RUN ln -s /usr/local/lib/liblustreapi.so /usr/local/lib/liblustreapi.so.1 && ldconfig

EXPOSE 8080
ENTRYPOINT ["/usr/sbin/robinhood"]
