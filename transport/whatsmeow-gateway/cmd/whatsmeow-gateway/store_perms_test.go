package main

import (
	"os"
	"path/filepath"
	"syscall"
	"testing"
)

func modeOf(t *testing.T, path string) os.FileMode {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("stat %s: %v", path, err)
	}
	return info.Mode().Perm()
}

func TestStorePathFromDSN(t *testing.T) {
	cases := map[string]string{
		"/state/wa-store.db":                                    "/state/wa-store.db",
		"file:/state/wa-store.db":                               "/state/wa-store.db",
		"file:/state/wa-store.db?_pragma=busy_timeout(10000)":   "/state/wa-store.db",
		"file:/state/wa-store.db?_pragma=journal_mode(WAL)&x=1": "/state/wa-store.db",
	}
	for dsn, want := range cases {
		if got := storePathFromDSN(dsn); got != want {
			t.Errorf("storePathFromDSN(%q) = %q, want %q", dsn, got, want)
		}
	}
}

func TestSecureStorePathRestrictsCredentialFiles(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "whatsapp")
	if err := os.Mkdir(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	db := filepath.Join(dir, "wa-store.db")
	// simulate the reported live state: 0644 store plus WAL/SHM companions
	for _, name := range []string{"wa-store.db", "wa-store.db-wal", "wa-store.db-shm", "wa-store.db-journal"} {
		if err := os.WriteFile(filepath.Join(dir, name), []byte("x"), 0o644); err != nil {
			t.Fatal(err)
		}
	}

	if err := secureStorePath(db); err != nil {
		t.Fatalf("secureStorePath: %v", err)
	}

	if got := modeOf(t, dir); got != 0o700 {
		t.Errorf("state dir mode = %04o, want 0700", got)
	}
	for _, name := range []string{"wa-store.db", "wa-store.db-wal", "wa-store.db-shm", "wa-store.db-journal"} {
		if got := modeOf(t, filepath.Join(dir, name)); got != 0o600 {
			t.Errorf("%s mode = %04o, want 0600", name, got)
		}
	}
}

func TestSecureStorePathToleratesMissingFiles(t *testing.T) {
	dir := t.TempDir()
	db := filepath.Join(dir, "wa-store.db")
	if err := secureStorePath(db); err != nil {
		t.Fatalf("a store that does not exist yet must be fine: %v", err)
	}
	if _, err := os.Stat(db); !os.IsNotExist(err) {
		t.Fatalf("secureStorePath must not create the store")
	}
}

func TestApplyPrivateUmaskMakesNewFilesPrivate(t *testing.T) {
	previous := syscall.Umask(0o022)
	defer syscall.Umask(previous)

	applyPrivateUmask()
	path := filepath.Join(t.TempDir(), "created.db")
	if err := os.WriteFile(path, []byte("x"), 0o666); err != nil {
		t.Fatal(err)
	}
	if got := modeOf(t, path); got != 0o600 {
		t.Fatalf("file created under the private umask = %04o, want 0600", got)
	}
}
