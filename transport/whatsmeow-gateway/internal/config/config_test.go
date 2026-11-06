package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeConfig(t *testing.T, body string) string {
	t.Helper()
	dir := t.TempDir()
	path := filepath.Join(dir, "gateway.json")
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	state := filepath.Join(dir, "state")
	if err := os.Mkdir(state, 0o700); err != nil {
		t.Fatal(err)
	}
	return path
}

const minimal = `{
  "version": 1,
  "broker_socket": "broker.sock",
  "allowlist_path": "allowlist.json",
  "state_dir": "state",
  "limits": {"max_message_chars": 100, "max_reply_chars": 200}
}`

func TestLoadResolvesRelativePaths(t *testing.T) {
	path := writeConfig(t, minimal)
	cfg, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if !filepath.IsAbs(cfg.BrokerSocket) || !filepath.IsAbs(cfg.StateDir) {
		t.Fatalf("paths not resolved: %+v", cfg)
	}
	if cfg.Limits.BrokerTimeoutSec <= 0 {
		t.Fatal("defaults not applied")
	}
	if !strings.Contains(cfg.StoreDSN, cfg.StateDir) {
		t.Fatalf("store dsn %q not inside state dir %q", cfg.StoreDSN, cfg.StateDir)
	}
}

func TestLoadRejectsPlaceholders(t *testing.T) {
	body := strings.Replace(minimal, `"broker_socket": "broker.sock"`, `"broker_socket": "__REQUIRED__"`, 1)
	if _, err := Load(writeConfig(t, body)); err == nil || !strings.Contains(err.Error(), "placeholder") {
		t.Fatalf("expected placeholder rejection, got %v", err)
	}
}

func TestLoadRejectsUnknownFields(t *testing.T) {
	body := strings.Replace(minimal, `"version": 1,`, `"version": 1, "extra": true,`, 1)
	if _, err := Load(writeConfig(t, body)); err == nil {
		t.Fatal("unknown key must be rejected")
	}
}

func TestLoadRejectsPublicStateDir(t *testing.T) {
	path := writeConfig(t, minimal)
	state := filepath.Join(filepath.Dir(path), "state")
	if err := os.Chmod(state, 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := Load(path); err == nil || !strings.Contains(err.Error(), "group/world accessible") {
		t.Fatalf("expected state dir permission rejection, got %v", err)
	}
}

func TestLoadRejectsOversizedMessageLimit(t *testing.T) {
	body := strings.Replace(minimal, `"max_message_chars": 100`, `"max_message_chars": 999999`, 1)
	if _, err := Load(writeConfig(t, body)); err == nil {
		t.Fatal("absurd message limit must be rejected")
	}
}

func TestLoadRejectsUnsupportedDialect(t *testing.T) {
	body := strings.Replace(minimal, `"version": 1,`, `"version": 1, "dialect": "mysql",`, 1)
	if _, err := Load(writeConfig(t, body)); err == nil {
		t.Fatal("unsupported dialect must be rejected")
	}
}
