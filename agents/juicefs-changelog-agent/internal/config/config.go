package config

import (
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"strings"

	"gopkg.in/yaml.v3"
)

type Config struct {
	Listen     string            `yaml:"listen"`
	CursorDir  string            `yaml:"cursor_dir"`
	MaxStreams uint32            `yaml:"max_streams"`
	Volumes    map[string]Volume `yaml:"volumes"`
}

type Volume struct {
	MetaURL string `yaml:"meta_url"`
}

func Load(path string) (Config, error) {
	f, err := os.Open(path)
	if err != nil {
		return Config{}, err
	}
	defer f.Close()
	return Decode(f)
}

func Decode(r io.Reader) (Config, error) {
	var cfg Config
	dec := yaml.NewDecoder(r)
	dec.KnownFields(true)
	if err := dec.Decode(&cfg); err != nil {
		return Config{}, fmt.Errorf("decode config: %w", err)
	}
	if err := cfg.Validate(); err != nil {
		return Config{}, err
	}
	return cfg, nil
}

func (c *Config) Validate() error {
	if c.Listen == "" {
		return errors.New("listen is required")
	}
	host, _, err := net.SplitHostPort(c.Listen)
	if err != nil {
		return fmt.Errorf("listen must be host:port: %w", err)
	}
	if host == "" || host == "0.0.0.0" || host == "::" {
		return errors.New("listen must use an explicit non-wildcard address")
	}
	if c.CursorDir == "" {
		return errors.New("cursor_dir is required")
	}
	if c.MaxStreams == 0 {
		return errors.New("max_streams must be greater than zero")
	}
	if len(c.Volumes) == 0 {
		return errors.New("at least one volume is required")
	}
	for name, volume := range c.Volumes {
		if strings.TrimSpace(name) == "" {
			return errors.New("volume name cannot be empty")
		}
		if !strings.HasPrefix(volume.MetaURL, "fdb://") {
			return fmt.Errorf("volume %q meta_url must use fdb://", name)
		}
		if !strings.Contains(volume.MetaURL, "prefix=") {
			return fmt.Errorf("volume %q meta_url must include an explicit prefix", name)
		}
	}
	return nil
}
