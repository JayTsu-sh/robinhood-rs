package server

import (
	"context"
	"errors"
	"io"
	"log/slog"
	"testing"

	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/cursor"
	changelogv1 "github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/gen/changelogv1"
	"github.com/JayTsu-sh/robinhood-rs/agents/juicefs-changelog-agent/internal/source"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

type fakeScanner struct {
	records []*changelogv1.ChangelogRecord
}

func (f fakeScanner) Scan(ctx context.Context, after int64, emit func(int64, string) error) error {
	for _, record := range f.records {
		if record.Version > after {
			if err := emit(record.Version, record.Entry); err != nil {
				return err
			}
		}
	}
	return nil
}

type fakeStream struct {
	ctx     context.Context
	records []*changelogv1.ChangelogRecord
}

func (f *fakeStream) Send(record *changelogv1.ChangelogRecord) error {
	f.records = append(f.records, record)
	return nil
}
func (*fakeStream) SetHeader(metadata.MD) error  { return nil }
func (*fakeStream) SendHeader(metadata.MD) error { return nil }
func (*fakeStream) SetTrailer(metadata.MD)       {}
func (*fakeStream) SendMsg(any) error            { return nil }
func (*fakeStream) RecvMsg(any) error            { return io.EOF }
func (f *fakeStream) Context() context.Context   { return f.ctx }

func TestWatchAllowlistedVolumeAndCursor(t *testing.T) {
	store, _ := cursor.NewFileStore(t.TempDir())
	_ = store.Commit(context.Background(), "jfs-nfs", 10)
	s := New(map[string]source.Scanner{"jfs-nfs": fakeScanner{records: []*changelogv1.ChangelogRecord{
		{Version: 10, Entry: "old"}, {Version: 11, Entry: "new"},
	}}}, store, 1, slog.New(slog.NewTextHandler(io.Discard, nil)))
	stream := &fakeStream{ctx: context.Background()}
	if err := s.Watch(&changelogv1.WatchRequest{Volume: "jfs-nfs"}, stream); err != nil {
		t.Fatal(err)
	}
	if len(stream.records) != 1 || stream.records[0].Version != 11 || stream.records[0].Volume != "jfs-nfs" {
		t.Fatalf("unexpected records: %+v", stream.records)
	}
	if _, err := s.Ack(context.Background(), &changelogv1.AckRequest{Volume: "jfs-nfs", Version: 11}); err != nil {
		t.Fatal(err)
	}
	if got, _ := store.Get(context.Background(), "jfs-nfs"); got != 11 {
		t.Fatalf("cursor not committed: %d", got)
	}
}

func TestWatchRejectsUnknownVolume(t *testing.T) {
	store, _ := cursor.NewFileStore(t.TempDir())
	s := New(map[string]source.Scanner{}, store, 1, slog.New(slog.NewTextHandler(io.Discard, nil)))
	err := s.Watch(&changelogv1.WatchRequest{Volume: "attacker"}, &fakeStream{ctx: context.Background()})
	if status.Code(err) != codes.NotFound {
		t.Fatalf("expected NotFound, got %v", err)
	}
}

func TestWatchHidesSourceErrors(t *testing.T) {
	store, _ := cursor.NewFileStore(t.TempDir())
	s := New(map[string]source.Scanner{"jfs": errorScanner{}}, store, 1, slog.New(slog.NewTextHandler(io.Discard, nil)))
	err := s.Watch(&changelogv1.WatchRequest{Volume: "jfs"}, &fakeStream{ctx: context.Background()})
	if status.Code(err) != codes.Unavailable || errors.Is(err, errSecret) {
		t.Fatalf("expected sanitized Unavailable, got %v", err)
	}
}

func TestAckRejectsUndeliveredVersion(t *testing.T) {
	store, _ := cursor.NewFileStore(t.TempDir())
	s := New(map[string]source.Scanner{"jfs": fakeScanner{}}, store, 1, slog.New(slog.NewTextHandler(io.Discard, nil)))
	_, err := s.Ack(context.Background(), &changelogv1.AckRequest{Volume: "jfs", Version: 99})
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("expected FailedPrecondition, got %v", err)
	}
}

var errSecret = errors.New("secret metadata URL")

type errorScanner struct{}

func (errorScanner) Scan(context.Context, int64, func(int64, string) error) error { return errSecret }
