#!/usr/bin/env bash
# Build the development Compose image and test its binaries without Lustre.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROJECT="rbh-smoke-$BASHPID-$RANDOM"
compose=(docker compose --project-name "$PROJECT" --file "$ROOT/dev/compose/docker-compose.yml")
DB_IMAGE="$("${compose[@]}" config --images mariadb)"
RUNTIME_IMAGE="$PROJECT-rbh-daemon"
SMOKE_IMAGE="$PROJECT-test"
SMOKE_CONTAINER="$PROJECT-test"
db_id=""

cleanup() {
    docker stop "$SMOKE_CONTAINER" >/dev/null 2>&1 || true
    if [[ -n "$db_id" ]]; then
        docker stop "$db_id" >/dev/null || true
    fi
    docker image rm "$SMOKE_IMAGE" "$RUNTIME_IMAGE" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

"${compose[@]}" build rbh-daemon

# Add the test clients to a disposable image derived from the Compose image.
docker build --platform linux/amd64 --tag "$SMOKE_IMAGE" - <<DOCKERFILE
FROM $RUNTIME_IMAGE
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl mariadb-client \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /opt/rbh-smoke/release \
    && ln -s /usr/sbin/robinhood /usr/bin/rbh /opt/rbh-smoke/release/
ENV CARGO_TARGET_DIR=/opt/rbh-smoke
DOCKERFILE

# Only the smoke runner shares MariaDB's loopback interface.
db_id="$(docker run --detach --rm --network none --tmpfs /var/lib/mysql \
    --env MARIADB_ALLOW_EMPTY_ROOT_PASSWORD=1 "$DB_IMAGE")"
for attempt in $(seq 1 60); do
    if docker exec "$db_id" mariadb --no-defaults --host=127.0.0.1 --user=root \
        --skip-password --connect-timeout=1 --execute 'SELECT 1' >/dev/null 2>&1; then
        break
    fi
    if [[ "$attempt" == 60 ]]; then
        docker logs "$db_id" >&2
        printf 'MariaDB startup timed out\n' >&2
        exit 1
    fi
    sleep 1
done

docker run --interactive --rm --init --name "$SMOKE_CONTAINER" \
    --network "container:$db_id" --env LD_BIND_NOW=1 --env RBH_TEST_CONSOLE=1 \
    --env RBH_SCHEMA_FILE=/usr/share/robinhood/schema.sql \
    --entrypoint bash "$SMOKE_IMAGE" -s < "$ROOT/tests/e2e.sh"
