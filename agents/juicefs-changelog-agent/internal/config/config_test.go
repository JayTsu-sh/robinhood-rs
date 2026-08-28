package config

import (
	"strings"
	"testing"
)

const validConfig = `
listen: 10.131.9.41:9443
cursor_dir: /var/lib/juicefs-changelog-agent/cursors
max_streams: 4
volumes:
  jfs-nfs:
    meta_url: fdb:///etc/foundationdb/fdb.cluster?prefix=jfs_nfs
`

func TestDecodeValid(t *testing.T) {
	cfg, err := Decode(strings.NewReader(validConfig))
	if err != nil {
		t.Fatal(err)
	}
	if got := cfg.Volumes["jfs-nfs"].MetaURL; !strings.Contains(got, "prefix=jfs_nfs") {
		t.Fatalf("unexpected meta URL: %q", got)
	}
}

func TestRequiresCursorDir(t *testing.T) {
	_, err := Decode(strings.NewReader(strings.Replace(validConfig, "cursor_dir: /var/lib/juicefs-changelog-agent/cursors\n", "", 1)))
	if err == nil || !strings.Contains(err.Error(), "cursor_dir") {
		t.Fatalf("expected cursor_dir rejection, got %v", err)
	}
}

func TestRejectsWildcardListen(t *testing.T) {
	_, err := Decode(strings.NewReader(strings.Replace(validConfig, "10.131.9.41:9443", "0.0.0.0:9443", 1)))
	if err == nil || !strings.Contains(err.Error(), "non-wildcard") {
		t.Fatalf("expected wildcard rejection, got %v", err)
	}
}

func TestRejectsNonFDBVolume(t *testing.T) {
	_, err := Decode(strings.NewReader(strings.Replace(validConfig, "fdb:///", "redis://", 1)))
	if err == nil || !strings.Contains(err.Error(), "fdb://") {
		t.Fatalf("expected fdb rejection, got %v", err)
	}
}

func TestRejectsUnknownField(t *testing.T) {
	_, err := Decode(strings.NewReader(validConfig + "unexpected: true\n"))
	if err == nil {
		t.Fatal("expected unknown field rejection")
	}
}
