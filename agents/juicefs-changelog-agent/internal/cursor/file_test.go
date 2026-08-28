package cursor

import (
	"context"
	"testing"
)

func TestFileStorePersistsMonotonicCursor(t *testing.T) {
	dir := t.TempDir()
	store, err := NewFileStore(dir)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if got, err := store.Get(ctx, "jfs-nfs"); err != nil || got != 0 {
		t.Fatalf("initial cursor: got=%d err=%v", got, err)
	}
	if err := store.Commit(ctx, "jfs-nfs", 42); err != nil {
		t.Fatal(err)
	}
	reopened, _ := NewFileStore(dir)
	if got, err := reopened.Get(ctx, "jfs-nfs"); err != nil || got != 42 {
		t.Fatalf("persisted cursor: got=%d err=%v", got, err)
	}
	if err := reopened.Commit(ctx, "jfs-nfs", 41); err == nil {
		t.Fatal("expected backwards cursor rejection")
	}
}

func TestFileStoreRejectsUnsafeVolume(t *testing.T) {
	store, _ := NewFileStore(t.TempDir())
	if _, err := store.Get(context.Background(), "../escape"); err == nil {
		t.Fatal("expected unsafe volume rejection")
	}
}
