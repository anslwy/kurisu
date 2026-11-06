// Package config loads the gateway's JSON configuration.
//
// Same philosophy as the broker: no defaults for anything security relevant,
// placeholders are rejected, unknown keys are rejected, and the state directory
// (which will hold paired-device credentials) must not be readable by others.
package config

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// Placeholder marks a value that must be edited before use.
const Placeholder = "__REQUIRED__"

// Limits bounds message sizes and timeouts.
type Limits struct {
	MaxMessageChars  int `json:"max_message_chars"`
	MaxReplyChars    int `json:"max_reply_chars"`
	BrokerTimeoutSec int `json:"broker_timeout_seconds"`
	// BrokerRuntimeMaxSec mirrors the broker's limits.runtime_max_sec when set,
	// so a too-small gateway timeout (which would expire first and leave a late
	// ack/reply to desync the next exchange) is rejected at startup.
	BrokerRuntimeMaxSec int `json:"broker_runtime_max_seconds"`
	SendTimeoutSec      int `json:"send_timeout_seconds"`
	ReconnectDelaySec   int `json:"reconnect_delay_seconds"`
}

// Config is the whole gateway configuration.
type Config struct {
	// Comment lets the *.example.json files carry notes without breaking strict
	// decoding (unknown keys are rejected on purpose).
	Comment       string `json:"_comment,omitempty"`
	Version       int    `json:"version"`
	BrokerSocket  string `json:"broker_socket"`
	AllowlistPath string `json:"allowlist_path"`
	StateDir      string `json:"state_dir"`
	StoreDSN      string `json:"store_dsn"`
	Dialect       string `json:"dialect"`
	LogLevel      string `json:"log_level"`
	// OpenSignin enables the PoC model: any 1:1 sender may start the exact
	// /signin command, and the Google test-user list is the allowlist. Safe
	// default false; the PoC examples set true.
	OpenSignin bool   `json:"open_signin"`
	Limits     Limits `json:"limits"`
	// Path to write a status/audit JSONL file (no message text, no keys).
	AuditPath string `json:"audit_path"`
}

// Load reads, validates and resolves a config file.
func Load(path string) (*Config, error) {
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read config %s: %w", path, err)
	}
	if strings.Contains(string(raw), Placeholder) {
		return nil, fmt.Errorf("config %s still contains %s placeholders", path, Placeholder)
	}
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	dec.DisallowUnknownFields()
	var cfg Config
	if err := dec.Decode(&cfg); err != nil {
		return nil, fmt.Errorf("config %s: %w", path, err)
	}
	if cfg.Version != 1 {
		return nil, fmt.Errorf("config: expected version 1, got %d", cfg.Version)
	}
	if cfg.BrokerSocket == "" || cfg.AllowlistPath == "" || cfg.StateDir == "" {
		return nil, fmt.Errorf("config: broker_socket, allowlist_path and state_dir are required")
	}
	if cfg.Dialect == "" {
		cfg.Dialect = "sqlite"
	}
	switch cfg.Dialect {
	case "sqlite", "sqlite3":
	default:
		return nil, fmt.Errorf("config.dialect: unsupported %q", cfg.Dialect)
	}
	if cfg.Limits.MaxMessageChars <= 0 {
		cfg.Limits.MaxMessageChars = 4000
	}
	if cfg.Limits.MaxReplyChars <= 0 {
		cfg.Limits.MaxReplyChars = 8000
	}
	if cfg.Limits.BrokerTimeoutSec <= 0 {
		// Must exceed the broker's runtime_max_sec by a margin: if the gateway gives
		// up first, the late ack/reply lands in the next exchange and desyncs it.
		cfg.Limits.BrokerTimeoutSec = 360
	}
	if cfg.Limits.SendTimeoutSec <= 0 {
		cfg.Limits.SendTimeoutSec = 30
	}
	if cfg.Limits.ReconnectDelaySec <= 0 {
		cfg.Limits.ReconnectDelaySec = 5
	}
	if cfg.Limits.MaxMessageChars > 8000 {
		return nil, fmt.Errorf("config.limits.max_message_chars: %d is too large", cfg.Limits.MaxMessageChars)
	}
	if cfg.Limits.BrokerRuntimeMaxSec > 0 &&
		cfg.Limits.BrokerTimeoutSec < cfg.Limits.BrokerRuntimeMaxSec+60 {
		return nil, fmt.Errorf(
			"config.limits.broker_timeout_seconds (%d) must exceed broker_runtime_max_seconds (%d) by at least 60s",
			cfg.Limits.BrokerTimeoutSec, cfg.Limits.BrokerRuntimeMaxSec,
		)
	}
	// Resolve relative paths against the config file so the process can run from
	// any working directory.
	base, err := filepath.Abs(filepath.Dir(path))
	if err != nil {
		return nil, err
	}
	cfg.BrokerSocket = absFrom(base, cfg.BrokerSocket)
	cfg.AllowlistPath = absFrom(base, cfg.AllowlistPath)
	cfg.StateDir = absFrom(base, cfg.StateDir)
	cfg.AuditPath = absFrom(base, cfg.AuditPath)
	// Default and resolve the device store *after* state_dir resolution.
	if cfg.StoreDSN == "" {
		cfg.StoreDSN = filepath.Join(cfg.StateDir, "wa-store.db")
	} else if !filepath.IsAbs(cfg.StoreDSN) && !strings.HasPrefix(cfg.StoreDSN, "file:") {
		cfg.StoreDSN = filepath.Join(cfg.StateDir, cfg.StoreDSN)
	}
	if err := cfg.CheckStateDir(); err != nil {
		return nil, err
	}
	return &cfg, nil
}

func absFrom(base, path string) string {
	if path == "" || filepath.IsAbs(path) {
		return path
	}
	return filepath.Join(base, path)
}

// CheckStateDir refuses a state directory that others can read: it will contain
// the paired-device credentials for the secondary account.
func (c *Config) CheckStateDir() error {
	info, err := os.Stat(c.StateDir)
	if err != nil {
		if os.IsNotExist(err) {
			return fmt.Errorf("state_dir %s does not exist; create it with mode 0700", c.StateDir)
		}
		return err
	}
	if !info.IsDir() {
		return fmt.Errorf("state_dir %s is not a directory", c.StateDir)
	}
	if info.Mode().Perm()&0o077 != 0 {
		return fmt.Errorf("state_dir %s is group/world accessible (%04o); chmod 700 it", c.StateDir, info.Mode().Perm())
	}
	return nil
}

// EnsureStateDir creates the state directory with 0700 if missing.
func (c *Config) EnsureStateDir() error {
	if info, err := os.Stat(c.StateDir); err == nil {
		if info.Mode().Perm()&0o077 != 0 {
			return fmt.Errorf("state_dir %s is group/world accessible (%04o); chmod 700 it", c.StateDir, info.Mode().Perm())
		}
		return nil
	}
	return os.MkdirAll(c.StateDir, 0o700)
}
