// Package policy implements the WhatsApp identity allowlist.
//
// It is deliberately dependency-free so the security-relevant logic can be
// tested without a network, a database, or a paired device.
//
// Rules (kept byte-for-byte compatible with broker/guest_broker/allowlist.py):
//   - default deny: an empty allowlist denies everything;
//   - only one-to-one chats are acceptable (no groups, broadcasts, newsletters);
//   - a device suffix is stripped; the JID must otherwise match exactly;
//   - one WhatsApp identity maps to at most one user id, and vice versa.
package policy

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"regexp"
	"sort"
	"strings"
)

// Field limits are intentionally small: the gateway is untrusted input.
const (
	MaxMessageChars = 4000
	MaxIdentities   = 32
)

var (
	phoneJID = regexp.MustCompile(`^\d{6,20}$`)
	lidJID   = regexp.MustCompile(`^[A-Za-z0-9._-]{4,64}$`)
	userID   = regexp.MustCompile(`^[a-z0-9][a-z0-9_-]{1,31}$`)
)

// ErrDenied is returned for every non-allowlisted identity.
var ErrDenied = errors.New("identity is not allowlisted")

// NormalizeJID lowercases, strips a device/resource suffix and validates the
// server part. It never guesses: anything unusual is an error.
func NormalizeJID(raw string) (string, error) {
	text := strings.ToLower(strings.TrimSpace(raw))
	if text == "" {
		return "", errors.New("empty WhatsApp identity")
	}
	if i := strings.Index(text, "/"); i >= 0 {
		text = text[:i]
	}
	at := strings.LastIndex(text, "@")
	if at < 0 {
		return "", fmt.Errorf("identity %q is not a JID", raw)
	}
	user, server := text[:at], text[at+1:]
	switch server {
	case "g.us", "broadcast", "newsletter", "status":
		return "", fmt.Errorf("chat type %q is not permitted for the pilot", server)
	case "s.whatsapp.net", "lid":
	default:
		return "", fmt.Errorf("unsupported JID server %q", server)
	}
	if i := strings.Index(user, ":"); i >= 0 {
		user = user[:i]
	}
	// Canonicalize the optional human "+" prefix away (whatsmeow emits plain
	// digits). Must match broker/guest_broker/allowlist.py exactly.
	if server == "s.whatsapp.net" {
		user = strings.TrimLeft(user, "+")
	}
	if server == "s.whatsapp.net" && !phoneJID.MatchString(user) {
		return "", fmt.Errorf("malformed phone-number JID %q", raw)
	}
	if server == "lid" && !lidJID.MatchString(user) {
		return "", fmt.Errorf("malformed LID JID %q", raw)
	}
	return user + "@" + server, nil
}

// Identity is one allowlist row.
type Identity struct {
	WAJID   string `json:"wa_jid"`
	UserID  string `json:"user_id"`
	Enabled *bool  `json:"enabled,omitempty"`
	Note    string `json:"note,omitempty"`
}

func (i Identity) isEnabled() bool { return i.Enabled == nil || *i.Enabled }

type allowlistFile struct {
	Version    int        `json:"version"`
	Identities []Identity `json:"identities"`
}

// Allowlist is an immutable exact-match allowlist.
type Allowlist struct {
	byJID map[string]Identity
	users map[string]string
}

// New builds an allowlist, validating uniqueness in both directions.
func New(identities []Identity) (*Allowlist, error) {
	if len(identities) > MaxIdentities {
		return nil, fmt.Errorf("allowlist has %d identities (max %d)", len(identities), MaxIdentities)
	}
	a := &Allowlist{byJID: map[string]Identity{}, users: map[string]string{}}
	for _, id := range identities {
		jid, err := NormalizeJID(id.WAJID)
		if err != nil {
			return nil, err
		}
		if !userID.MatchString(id.UserID) {
			return nil, fmt.Errorf("invalid user_id %q", id.UserID)
		}
		if _, dup := a.byJID[jid]; dup {
			return nil, fmt.Errorf("duplicate identity %s", jid)
		}
		if _, dup := a.users[id.UserID]; dup {
			return nil, fmt.Errorf("user_id %q is claimed by two WhatsApp identities", id.UserID)
		}
		id.WAJID = jid
		a.byJID[jid] = id
		a.users[id.UserID] = jid
	}
	return a, nil
}

// Load reads and validates an allowlist JSON file.
//
// The file must be private (0600): it enumerates exactly who can reach a guest.
func Load(path string) (*Allowlist, error) {
	info, err := os.Stat(path)
	if err != nil {
		return nil, fmt.Errorf("allowlist %s: %w", path, err)
	}
	if info.Mode().Perm()&0o077 != 0 {
		return nil, fmt.Errorf("allowlist %s is group/world accessible (%04o); chmod 600 it", path, info.Mode().Perm())
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var file allowlistFile
	dec := json.NewDecoder(strings.NewReader(string(raw)))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&file); err != nil {
		return nil, fmt.Errorf("allowlist %s: %w", path, err)
	}
	if file.Version != 1 {
		return nil, fmt.Errorf("allowlist %s: expected version 1", path)
	}
	return New(file.Identities)
}

// Len returns the number of identities (0 means "deny everything").
func (a *Allowlist) Len() int { return len(a.byJID) }

// Resolve maps a sender JID to a user id, or ErrDenied.
func (a *Allowlist) Resolve(rawJID string) (string, error) {
	jid, err := NormalizeJID(rawJID)
	if err != nil {
		return "", ErrDenied
	}
	id, ok := a.byJID[jid]
	if !ok {
		return "", ErrDenied
	}
	if !id.isEnabled() {
		return "", ErrDenied
	}
	return id.UserID, nil
}

// Contains reports whether the identity may talk to the guest.
func (a *Allowlist) Contains(rawJID string) bool {
	_, err := a.Resolve(rawJID)
	return err == nil
}

// JIDs returns the normalized identities in sorted order.
func (a *Allowlist) JIDs() []string {
	out := make([]string, 0, len(a.byJID))
	for jid := range a.byJID {
		out = append(out, jid)
	}
	sort.Strings(out)
	return out
}

// Fingerprint matches protocol.allowlist_fingerprint in the Python broker:
// sha256 of "\n".join(sorted(normalized_jids)).
func Fingerprint(jids []string) string {
	sorted := append([]string(nil), jids...)
	sort.Strings(sorted)
	sum := sha256.Sum256([]byte(strings.Join(sorted, "\n")))
	return hex.EncodeToString(sum[:])
}

// Fingerprint returns the broker-compatible fingerprint of this allowlist.
func (a *Allowlist) Fingerprint() string { return Fingerprint(a.JIDs()) }

// Redacted returns a log-safe view: identities are masked.
func (a *Allowlist) Redacted() []string {
	out := make([]string, 0, len(a.byJID))
	for _, id := range a.byJID {
		user := id.WAJID
		server := ""
		if i := strings.Index(user, "@"); i >= 0 {
			user, server = user[:i], user[i:]
		}
		if len(user) > 3 {
			user = user[:3] + "***"
		}
		out = append(out, fmt.Sprintf("%s%s -> %s (enabled=%t)", user, server, id.UserID, id.isEnabled()))
	}
	sort.Strings(out)
	return out
}
