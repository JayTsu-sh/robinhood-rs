package cursor

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"sync"
)

var safeVolume = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`)

type FileStore struct {
	dir string
	mu  sync.Mutex
}

func NewFileStore(dir string) (*FileStore, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, fmt.Errorf("create cursor directory: %w", err)
	}
	return &FileStore{dir: dir}, nil
}

func (s *FileStore) Get(_ context.Context, volume string) (int64, error) {
	path, err := s.path(volume)
	if err != nil {
		return 0, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	return readCursor(path)
}

func (s *FileStore) Commit(_ context.Context, volume string, version int64) error {
	if version <= 0 {
		return fmt.Errorf("cursor must be positive")
	}
	path, err := s.path(volume)
	if err != nil {
		return err
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	current, err := readCursor(path)
	if err != nil {
		return err
	}
	if version < current {
		return fmt.Errorf("cursor cannot move backwards: stored=%d attempted=%d", current, version)
	}
	if version == current {
		return nil
	}

	tmp, err := os.CreateTemp(s.dir, ".cursor-*")
	if err != nil {
		return fmt.Errorf("create cursor temp file: %w", err)
	}
	tmpPath := tmp.Name()
	defer os.Remove(tmpPath)
	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return err
	}
	if _, err := fmt.Fprintf(tmp, "%d\n", version); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Sync(); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	if err := os.Rename(tmpPath, path); err != nil {
		return fmt.Errorf("replace cursor: %w", err)
	}
	dir, err := os.Open(s.dir)
	if err != nil {
		return err
	}
	defer dir.Close()
	return dir.Sync()
}

func (s *FileStore) path(volume string) (string, error) {
	if !safeVolume.MatchString(volume) {
		return "", fmt.Errorf("unsafe volume name")
	}
	return filepath.Join(s.dir, volume+".cursor"), nil
}

func readCursor(path string) (int64, error) {
	b, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return 0, nil
	}
	if err != nil {
		return 0, fmt.Errorf("read cursor: %w", err)
	}
	v, err := strconv.ParseInt(strings.TrimSpace(string(b)), 10, 64)
	if err != nil || v < 0 {
		return 0, fmt.Errorf("invalid stored cursor")
	}
	return v, nil
}
