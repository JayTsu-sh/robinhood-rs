#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'HELP'
Run the containerized console acceptance suite against a Lustre lab filesystem.

Usage: sudo env RBH_TEST_LUSTRE_SOURCE='<mgs-nid>:/<filesystem>' \
  RBH_TEST_LUSTRE_PARENT='<existing scratch directory relative to the root>' \
  bash tests/e2e-console-lustre.sh

Set RBH_CONTAINER_IMAGE to reuse an existing local image. Otherwise the runner
builds one from the current checkout. Logs and reports are saved in a new
local temporary directory printed at startup.
HELP
}

if [[ ${1:-} == --help ]]; then
    usage
    exit 0
fi
if [[ $# != 0 ]]; then usage >&2; exit 2; fi

fail() { printf '%s\n' "$*" >&2; exit 1; }
[[ $EUID == 0 && -c /dev/lnet ]] || fail 'Run as root on a Lustre client with /dev/lnet.'
source=${RBH_TEST_LUSTRE_SOURCE:?Set RBH_TEST_LUSTRE_SOURCE to the lab mount source.}
parent=${RBH_TEST_LUSTRE_PARENT:?Set RBH_TEST_LUSTRE_PARENT to a scratch directory on this filesystem.}
[[ $source =~ ^[^/[:space:]]+:/[A-Za-z0-9_-]+$ ]] || fail 'Expected a Lustre mount source in the form MGS-NID:/filesystem.'
[[ $parent =~ ^[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*$ && /$parent/ != */../* && /$parent/ != */./* ]] \
    || fail 'Use a canonical scratch directory relative to the Lustre mount root.'
endpoint=${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}' "$(docker context show)")}
[[ $endpoint == unix://* ]] || fail 'The runner requires a local Docker daemon.'

cd -- "$(dirname -- "$0")/.."
root=$PWD
work=$(mktemp -d /tmp/rbh-console-lustre.XXXXXXXX)
output=$work/output
mount=$work/portal/fs
project=rbh-console-lustre-$(basename "$work" | cut -d . -f 2 | tr '[:upper:]' '[:lower:]')
image=${RBH_CONTAINER_IMAGE:-$project-image}
database=$project-db
fixture=''
capabilities='{"changelog":false,"namespace":true,"purge":false,"hsm":true,"stripe":true,"ost":true}'
mkdir -p "$output" "$mount" "$work/config" "$work/client" "$work/browser/node_modules"

compose() (
    mode=$1; shift
    export RBH_CONTAINER_IMAGE=$image RBH_CONTAINER_CONFIG_DIR=$work/config
    export RBH_HSM_PRINCIPAL_FILE=/run/rbh-config/principal.json
    export RBH_CONSOLE_OBSERVER_ID=$project-$mode
    export RBH_DATABASE_URL="mysql://root:$db_password@127.0.0.1:${ports[0]}/rbh_$mode"
    export RBH_LISTEN_ADDR=127.0.0.1:${ports[$mode]}
    export RBH_FILESYSTEMS_JSON='[]'
    if [[ $mode != managed ]]; then
        RBH_FILESYSTEMS_JSON=$(jq -nc --arg id "$mode-fixture" --argjson caps "$capabilities" \
            '[{id:$id,backend:"lustre",mount_path:"/mnt/qa-lustre",capabilities:$caps}]')
    fi
    docker compose --project-name "$project-$mode" --file "$root/packaging/docker/compose.yml" \
        --file "$work/$mode.yml" "$@"
)

cleanup() {
    local status=$? mode target layer
    trap - EXIT
    set +e
    for mode in execution wizard managed; do
        if [[ -f $work/$mode.yml ]]; then
            docker rm -f "$project-browser-$mode" >/dev/null 2>&1
            compose "$mode" logs --no-color > "$output/$mode-daemon.log" 2>&1
            compose "$mode" down --timeout 15 >/dev/null 2>&1 || status=1
        fi
    done
    if docker container inspect "$database" >/dev/null 2>&1; then
        docker rm -f "$database" >/dev/null || status=1
    fi
    if [[ -n $fixture ]]; then
        if mountpoint -q "$mount" && [[ ! -L $fixture ]]; then
            find "$fixture" -xdev -depth -delete || status=1
        else
            printf 'Could not clean the Lustre fixture %s\n' "$fixture" >&2
            status=1
        fi
    fi
    for target in "$mount" "$work/portal"; do
        for layer in {1..4}; do
            mountpoint -q "$target" || break
            umount "$target" || { status=1; break; }
        done
        if mountpoint -q "$target"; then status=1; fi
    done
    if [[ -z ${RBH_CONTAINER_IMAGE:-} ]] && docker image inspect "$image" >/dev/null 2>&1; then
        docker image rm "$image" >/dev/null || status=1
    fi
    for target in "$work/db-password" "$work/config/writers.json"; do
        if [[ -f $target ]]; then rm -- "$target" || status=1; fi
    done
    if [[ -n ${RBH_CONSOLE_LUSTRE_TEST_TOKEN:-} ]]; then
        python3 - "$output" <<'PY' || status=1
from os import environ
from pathlib import Path
from sys import argv

token = environ['RBH_CONSOLE_LUSTRE_TEST_TOKEN'].encode()
for path in Path(argv[1]).rglob('*'):
    if path.suffix in {'.log', '.md', '.json'} and path.is_file():
        original = path.read_bytes()
        if token in original:
            path.write_bytes(original.replace(token, b'<redacted>'))
PY
    fi
    printf 'Results: %s\n' "$output"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'Results: %s\n' "$output"

if [[ -n ${RBH_CONTAINER_IMAGE:-} ]]; then
    docker image inspect "$image" >/dev/null
else
    docker build --progress=plain --file dev/compose/Dockerfile --tag "$image" . 2>&1 | tee "$output/build.log"
fi
docker image inspect "$image" --format '{{.Id}}' > "$output/image-id"

mount --bind "$work/portal" "$work/portal"
mount --make-rshared "$work/portal"
docker run --rm --network host --cap-add SYS_ADMIN --cap-add NET_ADMIN --device /dev/lnet \
    --security-opt apparmor=unconfined --mount "type=bind,src=$work/portal,dst=/portal,bind-propagation=rshared" \
    --entrypoint /bin/sh "$image" -ec 'mount -t lustre "$1" /portal/fs' _ "$source"
mountpoint -q "$mount" && [[ $(findmnt -n -o FSTYPE --target "$mount" | sort -u) == lustre ]] \
    || fail 'The isolated Lustre mount did not propagate to the host.'
[[ -d $mount/$parent && $(realpath -e -- "$mount/$parent") == "$mount/$parent" &&
    $(findmnt -n -o TARGET --target "$mount/$parent" | sort -u) == "$mount" ]] \
    || fail 'The scratch directory must already exist on the mounted filesystem.'
fixture=$(mktemp -d "$mount/$parent/rbh-console.XXXXXXXX")
execution_path=${fixture#"$mount/"}/execution
mkdir "$fixture/execution" "$fixture/wizard"

# Export lfs and its loader from the same image as the daemon. Playwright's
# Ubuntu base and the daemon's Debian base ship different library versions.
docker run --rm --network none --entrypoint /bin/sh "$image" -ec '
    cd /
    ldd /usr/bin/lfs | awk '\''/=> \/|^[[:space:]]*\// {for(i=1;i<=NF;i++) if($i ~ /^\//) print substr($i, 2)}'\'' |
        tar -chf - usr/bin/lfs -T -
' | tar -xf - --transform='s|.*/||' -C "$work/client"
cat > "$work/client/run-lfs" <<'HELPER'
#!/bin/sh
exec /qa-client/ld-linux-x86-64.so.2 --library-path /qa-client /qa-client/lfs "$@"
HELPER
chmod +x "$work/client/run-lfs"

docker run --rm --network host --cap-add DAC_READ_SEARCH --device /dev/lnet \
    --mount "type=bind,src=$fixture/wizard,dst=/wizard" --entrypoint /bin/sh "$image" -ec '
        mkdir /wizard/team /wizard/outside
        for size in 100 200 300; do
            file=/wizard/team/file-$size
            lfs setstripe -p hot -c 1 "$file"
            dd if=/dev/zero of="$file" bs="$size" count=1 status=none
        done
        ln /wizard/team/file-300 /wizard/outside/outside-link
        touch -d "1 hour ago" /wizard/team/file-*
    '

port_output=$(python3 - <<'PY'
from socket import socket
ports = [socket() for _ in range(4)]
for sock in ports:
    sock.bind(('127.0.0.1', 0))
print(*(sock.getsockname()[1] for sock in ports), sep='\n')
PY
)
mapfile -t selected_ports <<< "$port_output"
declare -A ports=([0]=${selected_ports[0]} [execution]=${selected_ports[1]} [wizard]=${selected_ports[2]} [managed]=${selected_ports[3]})
umask 077
db_password=$(openssl rand -hex 20)
printf '%s\n' "$db_password" > "$work/db-password"
export RBH_CONSOLE_LUSTRE_TEST_TOKEN=$(openssl rand -hex 24)
printf '{"qa-operator":"%s"}\n' "$(printf '%s' "$RBH_CONSOLE_LUSTRE_TEST_TOKEN" | sha256sum | cut -d ' ' -f 1)" > "$work/config/writers.json"
printf '%s\n' '{"serviceAccount":"tiering-daemon","roles":[]}' > "$work/config/principal.json"

# Only this disposable DB and the daemons use these loopback ports.
docker run -d --name "$database" --network host --tmpfs /var/lib/mysql:size=768m \
    --mount "type=bind,src=$work/db-password,dst=/run/db-password,readonly" \
    -e MARIADB_ROOT_PASSWORD_FILE=/run/db-password mariadb:11 \
    --bind-address=127.0.0.1 --port="${ports[0]}" >/dev/null

sql() {
    docker exec -i "$database" sh -ec 'MYSQL_PWD=$(cat /run/db-password) exec mariadb -uroot "$@"' _ "$@"
}
for attempt in {1..45}; do
    if sql --protocol=tcp --host=127.0.0.1 --port="${ports[0]}" -e 'SELECT 1' >/dev/null 2>&1; then break; fi
    if [[ $attempt == 45 ]]; then docker logs "$database" >&2; fail 'MariaDB startup timed out.'; fi
    sleep 1
done
for mode in execution wizard managed; do
    sql -e "CREATE DATABASE rbh_$mode"
    docker run --rm --network none --entrypoint cat "$image" /usr/share/robinhood/schema.sql | sql "rbh_$mode"
done

docker run --rm --network host --cap-add DAC_READ_SEARCH --device /dev/lnet \
    --mount "type=bind,src=$mount,dst=/mnt/catalog,readonly" --entrypoint /bin/bash "$image" -ec '
        IFS=/ read -r -a components <<< "$1"
        directory=.
        printf "%s\t%s\n" "$directory" "$(lfs path2fid /mnt/catalog)"
        for component in "${components[@]}"; do
            directory="$directory/$component"
            printf "%s\t%s\n" "$directory" "$(lfs path2fid "/mnt/catalog/$directory")"
        done
    ' _ "$execution_path" |
    python3 tests/fixtures/console-lustre-catalog.py "$mount" "$capabilities" |
    sql rbh_execution

playwright_version=$(jq -er '.packages["node_modules/@playwright/test"].version' web/package-lock.json)
playwright_image=mcr.microsoft.com/playwright:v$playwright_version-noble
cp web/{package.json,package-lock.json,playwright.config.ts} "$work/browser/"
cp -R web/tests "$work/browser/"
if [[ -f web/node_modules/@playwright/test/package.json ]] &&
    jq -e --arg version "$playwright_version" '.version == $version' web/node_modules/@playwright/test/package.json >/dev/null; then
    modules=$root/web/node_modules
else
    docker run --rm --mount "type=bind,src=$work/browser,dst=/app" --workdir /app "$playwright_image" \
        npm ci --ignore-scripts --no-audit --no-fund 2>&1 | tee "$output/npm-ci.log"
    modules=$work/browser/node_modules
fi

wait_api() {
    local mode=$1 path=$2 assertion=$3
    local url=http://127.0.0.1:${ports[$mode]}
    for attempt in {1..45}; do
        if curl --noproxy '*' -fsS --max-time 2 "$url$path" 2>/dev/null | jq -e "$assertion" >/dev/null; then return; fi
        sleep 1
    done
    fail "$mode daemon did not become ready at $path."
}

start_daemon() {
    local mode=$1 bind=$2 target=$3 read_only=$4
    cat > "$work/$mode.yml" <<YAML
services:
  rbh-daemon:
    environment:
      RBH_HSM_POLL_SECS: "0"
    volumes:
      - type: bind
        source: "$bind"
        target: "$target"
        read_only: $read_only
        bind:
          create_host_path: false
YAML
    compose "$mode" up -d --no-build --pull never
    wait_api "$mode" /api/health '.status == "ok"'
    if [[ $mode != managed ]]; then
        wait_api "$mode" /api/filesystems 'any(.[]; .tieringAvailable and ([.pools[] | select(.eligible).name] | index("hot") != null and index("cold") != null))'
    fi
}

browser() (
    local mode=$1 label=$2; shift 2
    local -a lustre=()
    if [[ $mode == execution ]]; then
        lustre=(--cap-add SYS_ADMIN --cap-add DAC_READ_SEARCH --device /dev/lnet
            --mount "type=bind,src=$mount,dst=/mnt/qa-lustre")
    fi
    export RBH_CONSOLE_WIZARD_TEST_TOKEN=$RBH_CONSOLE_LUSTRE_TEST_TOKEN
    export RBH_CONSOLE_EXECUTION_TEST_TOKEN=$RBH_CONSOLE_LUSTRE_TEST_TOKEN
    docker run --rm --init --name "$project-browser-$mode" --network host --ipc=host "${lustre[@]}" \
        --mount "type=bind,src=$work/browser,dst=/app,readonly" \
        --mount "type=bind,src=$modules,dst=/app/node_modules,readonly" \
        --mount "type=bind,src=$work/client,dst=/qa-client,readonly" \
        --mount "type=bind,src=$output,dst=/output" \
        -e "RBH_CONSOLE_TEST_URL=http://127.0.0.1:${ports[$mode]}" \
        -e RBH_CONSOLE_LUSTRE_TEST_TOKEN -e "RBH_CONSOLE_LUSTRE_MOUNT_SOURCE=$source" \
        -e RBH_CONSOLE_WIZARD_TEST_TOKEN -e RBH_CONSOLE_WIZARD_TEST_FS=wizard-fixture \
        -e RBH_CONSOLE_EXECUTION_TEST_TOKEN -e RBH_CONSOLE_EXECUTION_TEST_FS=execution-fixture \
        -e RBH_CONSOLE_EXECUTION_TEST_MOUNT=/mnt/qa-lustre -e "RBH_CONSOLE_EXECUTION_TEST_ROOT=/mnt/qa-lustre/$execution_path" \
        -e RBH_CONSOLE_REQUIRE_LIVE_DATA -e RBH_CONSOLE_RUN_HISTORY_TEST \
        -w /app "$playwright_image" bash -ec '
            mkdir -p /tmp/qa-bin
            ln -s /qa-client/run-lfs /tmp/qa-bin/lfs
            export PATH=/tmp/qa-bin:$PATH
            exec npm run test:e2e -- "$@"
        ' _ "$@" --reporter=list --trace=off --output="/output/$label" 2>&1 | tee "$output/$label.log"
)

start_daemon execution "$mount" /mnt/qa-lustre false
browser execution migration policy-execution.spec.ts --workers=1

origin=http://127.0.0.1:${ports[execution]}
get() { curl --noproxy '*' -fsS --max-time 15 "$origin$1"; }
runs=$(get '/api/runs?state=finished' | jq -er '.items | select(length == 4) | sort_by(.id) | .[].idExact')
while IFS= read -r run; do
    get "/api/runs/$run/reports/run" > "$output/migration-report-$run.json"
done <<< "$runs"
jq -s 'map({run:.run.idExact, trigger:(if .run.requestedBy == null then "periodic" else "manual" end),
    state:.run.state, completedBytes:.operationResults.completedLogicalBytes,
    completedPools:[.operations[] | select(.state == "completed") | .completionEvidence.pool]})' \
    "$output"/migration-report-*.json > "$output/migration-summary.json"
jq -e 'length == 4 and (map(.completedPools) | flatten | length == 5 and all(.[]; . == "cold"))' \
    "$output/migration-summary.json" >/dev/null

policy=$(jq -er '.run.policyIdExact' "$output/migration-report-${runs%%$'\n'*}.json")
get "/api/policies/$policy" | jq -e '.definition | .metadata.name="qa-explicit-pools" |
    .spec.enabled=false | .spec.rules.entries[0].action //= .spec.rules.defaults.action' > "$work/read-policy.json"
printf 'header = "Authorization: Bearer %s"\n' "$RBH_CONSOLE_LUSTRE_TEST_TOKEN" |
    curl --config - --noproxy '*' -fsS --max-time 15 --json "@$work/read-policy.json" "$origin/api/policies" >/dev/null
RBH_CONSOLE_REQUIRE_LIVE_DATA=1 browser execution live-console console.spec.ts --grep 'the daemon serves the compiled console' --workers=1

sql rbh_execution < tests/fixtures/console-run-controls.sql
browser execution run-controls lustre-container.spec.ts --grep 'authenticates pause' --workers=1

start_daemon wizard "$fixture/wizard" /mnt/qa-lustre false
sql rbh_wizard < tests/fixtures/console-run-history.sql
RBH_CONSOLE_RUN_HISTORY_TEST=1 browser wizard wizard-and-history console.spec.ts policy-wizard.spec.ts resource-details.spec.ts \
    run-history.spec.ts --grep-invert 'the daemon serves the compiled console' --workers=3

start_daemon managed "$mount" /mnt/qa-existing true
browser managed create-mount lustre-container.spec.ts --grep 'creates and manages' --workers=1
origin=http://127.0.0.1:${ports[managed]}
management=/api/filesystems/management
post() {
    local path=$1 payload=$2 response=$3
    printf 'header = "Authorization: Bearer %s"\n' "$RBH_CONSOLE_LUSTRE_TEST_TOKEN" |
        curl --config - --noproxy '*' -sS --max-time 20 -o "$response" -w '%{http_code}' --json "$payload" "$origin$path"
}

old_container=$(compose managed ps -q rbh-daemon)
wait_api managed '/api/scans?filesystem=qa-managed' 'all(.[]; .state != "running")'
docker exec -d "$old_container" sh -ec '
    cd /mnt/qa-managed
    touch /tmp/qa-busy-ready
    while [ ! -f /tmp/qa-busy-release ]; do sleep 1; done
'
for attempt in {1..40}; do
    if docker exec "$old_container" test -f /tmp/qa-busy-ready; then break; fi
    if [[ $attempt == 40 ]]; then fail 'Could not hold the managed mount busy.'; fi
    sleep 0.1
done
status=$(post "$management/qa-managed" '{"action":"unmount"}' "$output/busy-unmount.json")
docker exec "$old_container" touch /tmp/qa-busy-release
[[ $status == 400 ]] && jq -e '.error | contains("busy")' "$output/busy-unmount.json" >/dev/null \
    || fail "Expected Linux to reject a busy managed unmount, got HTTP $status."
get "$management" | jq -e 'any(.resources[]; .id == "qa-managed" and .autoMount and .desiredMounted)' \
    > "$output/busy-unmount-keeps-intent.json" || fail 'A failed unmount changed the restore intent.'
printf 'Busy unmount preserved automatic restoration.\n'

docker kill "$old_container" >/dev/null
compose managed up -d --force-recreate --no-build --pull never
[[ $(compose managed ps -q rbh-daemon) != "$old_container" ]] || fail 'The managed mount container was not replaced.'
wait_api managed /api/health '.status == "ok"'
get "$management" | jq -e 'any(.resources[]; .id == "qa-managed" and .mounted)' \
    > "$output/restored-mount.json" || fail 'The restored mount was missing from the first management response.'
wait_api managed /api/filesystems 'any(.[]; .id == "qa-managed" and .tieringAvailable)'
browser managed mount-lifecycle lustre-container.spec.ts --grep 'restores after|shows an actual external' --workers=1

compose managed up -d --force-recreate --no-build --pull never
wait_api managed /api/health '.status == "ok"'
get "$management" | jq -e 'any(.resources[]; .id == "qa-managed" and .autoMount and .desiredMounted == false and .mounted == false)' \
    > "$output/unmounted-after-restart.json" || fail 'A manually unmounted cluster was lost or remounted.'

status=$(post "$management" '{"id":"qa-external","name":"Registration race","mount":{"type":"existing","path":"/mnt/qa-existing"}}' \
    "$output/race-register.json")
[[ $status == 201 ]] || fail "Could not register the external mount, got HTTP $status."
wait_api managed '/api/scans?filesystem=qa-external' 'all(.[]; .state != "running")'
jq '.metadata.name="qa-registration-race" | .spec.target.filesystem="qa-external" | del(.spec.target.directoryScope) |
    .spec.execution.activation.mode="preview" |
    .spec.governance={authorizationRef:"builtin-preview",approvalPolicyRef:"builtin-preview"}' \
    "$work/read-policy.json" > "$work/race-policy.json"

# Queue removal first and wait until both HTTP writes are blocked on the row.
coproc CONFIGURATION_LOCK { sql rbh_managed --batch --skip-column-names --unbuffered > "$work/configuration-lock.log"; }
lock_holder=$CONFIGURATION_LOCK_PID
printf 'START TRANSACTION; SELECT id FROM policy_configuration_lock WHERE id=1 FOR UPDATE;\n' >&"${CONFIGURATION_LOCK[1]}"
for attempt in {1..80}; do
    if [[ -s $work/configuration-lock.log ]]; then break; fi
    if [[ $attempt == 80 ]]; then fail 'Could not hold the policy configuration lock.'; fi
    sleep 0.1
done
wait_for_configuration_writers() {
    local expected=$1 count
    for attempt in {1..80}; do
        count=$(sql rbh_managed --batch --skip-column-names -e '
            SELECT COUNT(DISTINCT trx.trx_id)
            FROM information_schema.INNODB_LOCK_WAITS waits
            JOIN information_schema.INNODB_TRX trx ON trx.trx_id=waits.requesting_trx_id
            WHERE trx.trx_query LIKE "SELECT id FROM policy_configuration_lock%FOR UPDATE"')
        if [[ $count == "$expected" ]]; then return; fi
        sleep 0.1
    done
    fail "Expected $expected requests waiting for the configuration lock, found $count."
}
post "$management/qa-external" '{"action":"remove"}' "$output/race-remove.json" > "$output/race-remove-status" &
remover=$!
wait_for_configuration_writers 1
post /api/policies "@$work/race-policy.json" "$output/race-save.json" > "$output/race-save-status" &
saver=$!
wait_for_configuration_writers 2
printf 'COMMIT;\nQUIT\n' >&"${CONFIGURATION_LOCK[1]}"
wait "$lock_holder" "$remover" "$saver"
[[ $(< "$output/race-remove-status") == 204 ]] || fail 'Concurrent resource removal failed.'
[[ $(< "$output/race-save-status") == 400 ]] &&
    jq -e '.error | contains("filesystem qa-external is not configured")' "$output/race-save.json" >/dev/null \
    || fail "A policy save raced resource removal, got HTTP $(< "$output/race-save-status")."
printf 'The policy save queued behind removal rejected the removed registration.\n'
printf 'All containerized Lustre console scenarios passed.\n'
