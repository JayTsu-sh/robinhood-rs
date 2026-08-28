# JuiceFS changelog agent

This agent exposes JuiceFS Community Edition's Go `ScanChangelog` interface as
an agent-managed, server-streaming gRPC interface for Robinhood. It is intended
to run beside the JuiceFS metadata client on the JuiceFS VM.

## Security model

The external interface has two operations: `Watch(volume)` and
`Ack(volume, version)`.

- gRPC is plaintext by deployment choice. Bind only to an explicit management
  address and enforce the Robinhood source address with a host firewall.
- `volume` is resolved through a static administrator-controlled allowlist.
- RPC callers cannot supply a metadata URL, FoundationDB cluster file, key
  range, retention setting, or mutation.
- Streams and message sizes are bounded.
- Source errors returned to clients are sanitized.
- The agent durably owns one monotonic cursor per volume. Robinhood explicitly
  acknowledges records only after its own durable processing succeeds.
- Only one active Watch stream is allowed for each volume.

This is least privilege at the process and RPC layers except transport
authentication, which is intentionally delegated to network policy.
FoundationDB does not provide prefix-level authorization for the cluster file used by this
deployment: the agent's database credential can technically address more than
one prefix. OS isolation, code review, and keeping the cluster file only on the
JuiceFS VM remain required controls.

## Build

The build host does **not** need a complete JuiceFS VM, a JuiceFS mount,
FoundationDB Server, a cluster file, or connectivity to a live FoundationDB
cluster. The minimum build requirements are:

- Go 1.25 or newer (as declared by `go.mod`);
- a C compiler and standard build tools for CGO;
- the FoundationDB header `foundationdb/fdb_c.h`;
- the FoundationDB client library `libfdb_c.so`.

The JuiceFS Go source is downloaded by `go mod download`; this module pins it
to v1.4.1. On the validated Ubuntu 24.04 runtime, the official
`foundationdb-clients` 7.3.79 package provides both `fdb_c.h` and
`libfdb_c.so`. Many Ubuntu installations must first configure FoundationDB's
package repository or install its official client `.deb`.

```bash
sudo apt-get update
sudo apt-get install -y build-essential ca-certificates foundationdb-clients

go version
test -r /usr/include/foundationdb/fdb_c.h
ldconfig -p | grep libfdb_c.so

go mod download
make test
make build
```

Do not install `foundationdb-server` on a build-only host. Distribution package
names vary outside Ubuntu/Debian; the required artifacts are the header and
client library listed above. If they use non-system paths, build with:

```bash
CGO_CFLAGS="-I/opt/foundationdb/include" \
CGO_LDFLAGS="-L/opt/foundationdb/lib -lfdb_c" \
make build
```

The generated protobuf sources are committed, so `protoc` is not part of the
minimum build. It is required only when changing `api/v1/changelog.proto` and
running `make proto`.

The runtime requirements are separate: the deployment host needs
`libfdb_c.so`, a readable `/etc/foundationdb/fdb.cluster`, network access to
FoundationDB, and changelog enabled for each configured JuiceFS volume. The
Agent itself does not require a JuiceFS FUSE mount; Robinhood's baseline scan
and namespace verification do.

Common build failures:

- `foundationdb/fdb_c.h: No such file or directory`: install the FoundationDB
  client development files or correct `CGO_CFLAGS`.
- `cannot find -lfdb_c`: install the FoundationDB client library or correct
  `CGO_LDFLAGS`.

## Configure

```bash
install -d -o root -g juicefs-changelog-agent -m 0750 /etc/juicefs-changelog-agent
install -o root -g juicefs-changelog-agent -m 0640 config.example.yaml /etc/juicefs-changelog-agent/config.yaml
chown foundationdb:foundationdb /etc/foundationdb/fdb.cluster
chmod 0640 /etc/foundationdb/fdb.cluster
```

The systemd unit gives the Agent process the supplementary `foundationdb`
group; no other local user should be able to read the cluster file.

Allow TCP port 9443 only from the Robinhood host. Do not expose this plaintext
listener outside the isolated management network. Apply a host or upstream
firewall rule allowing only `10.131.9.10/32`; adjust that address if Robinhood
moves. Packaged nftables and systemd files apply this rule only to TCP 9443 and
leave all unrelated host traffic unchanged.

## Cursor contract

On first use, an absent cursor starts at the current tail. On later connections,
Watch resumes strictly after the Agent's durable cursor. Robinhood processes a
record durably and then calls `Ack(volume, version)`. The Agent rejects an ACK
for a version it has not delivered and persists accepted versions with
write-fsync-rename-directory-fsync. Duplicate delivery must be harmless. If
downtime exceeds JuiceFS retention and the cursor
falls behind the oldest retained record, rebuild from a full scan rather than
silently advancing the cursor.

## Robinhood client

Generate a tonic client from [`api/v1/changelog.proto`](api/v1/changelog.proto)
and call the server-streaming method:

```text
WatchRequest { volume: "jfs-nfs" }
```

For each record, durably process `entry`, then call
`AckRequest { volume: "jfs-nfs", version: record.version }`. The Agent maintains
independent cursor files for `jfs-nfs` and `jfs-s3`.

## Verification

```bash
go test ./...
go test -race ./internal/config ./internal/cursor ./internal/server
go vet ./...

# Standalone validation receiver; prints JSON and ACKs each printed record.
go run ./cmd/changelog-receiver \
  -address 10.131.9.41:9443 \
  -volume jfs-nfs
```

Production verification should connect from the firewall-allowed Robinhood
host, create a disposable file in the volume, and confirm
the stream contains the corresponding CREATE/WRITE/UNLINK sequence.
