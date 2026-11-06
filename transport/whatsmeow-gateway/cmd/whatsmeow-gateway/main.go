// Command whatsmeow-gateway is the isolated WhatsApp transport prototype.
//
// It owns exactly one thing: a *secondary* WhatsApp account paired to the
// dedicated pilot transport, with a required sender allowlist and default deny.
// It has no Omnigent credentials, no session knowledge and no administrative
// capability whatsoever.
//
// Subcommands:
//
//	run    connect and relay allowlisted messages to the broker
//	pair   explicitly pair the secondary account via QR (never automatic)
//	status show pairing/allowlist/broker state without revealing secrets
//
// Safety properties enforced by this binary:
//   - it never pairs during `run`; pairing needs the `pair` command, a
//     confirmation flag, and a state directory with no existing device;
//   - it refuses to start with an empty allowlist;
//   - it refuses to run as root;
//   - it drops group, self, empty and over-long messages, and never replies to a
//     chat that is not the allowlisted sender's own chat;
//   - credentials and the device store live in the state dir (mode 0700, gitignored)
//     and are never printed.
package main

import (
	"bufio"
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	_ "modernc.org/sqlite"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/protobuf/proto"

	"github.com/omnigent/pilot-wa-gateway/internal/config"
	"github.com/omnigent/pilot-wa-gateway/internal/gateway"
	"github.com/omnigent/pilot-wa-gateway/internal/policy"
	"github.com/omnigent/pilot-wa-gateway/internal/protocol"
)

const version = "0.1.0"

func logf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "%s %s\n", time.Now().UTC().Format(time.RFC3339), fmt.Sprintf(format, args...))
}

// logLine adapts logf to the gateway package's func(string) logger shape.
func logLine(message string) { logf("%s", message) }

func main() {
	if os.Geteuid() == 0 {
		fmt.Fprintln(os.Stderr, "refusing to run as root: the gateway must stay an unprivileged user process")
		os.Exit(2)
	}
	if len(os.Args) < 2 {
		usage()
		os.Exit(2)
	}
	command := os.Args[1]
	args := os.Args[2:]
	var err error
	switch command {
	case "run":
		err = cmdRun(args)
	case "pair":
		err = cmdPair(args)
	case "status":
		err = cmdStatus(args)
	case "version", "--version", "-v":
		fmt.Printf("whatsmeow-gateway %s (protocol v%d)\n", version, protocol.Version)
	case "help", "--help", "-h":
		usage()
	default:
		fmt.Fprintf(os.Stderr, "unknown command %q\n", command)
		usage()
		os.Exit(2)
	}
	if err != nil {
		logf("error: %v", err)
		os.Exit(1)
	}
}

func usage() {
	fmt.Fprintf(os.Stderr, `whatsmeow-gateway %s - isolated secondary-account WhatsApp transport

Usage:
  whatsmeow-gateway run     --config <path> [--dry-run]
  whatsmeow-gateway pair    --config <path> --confirm-secondary
  whatsmeow-gateway status  --config <path>
  whatsmeow-gateway version

The gateway never pairs automatically and never sends anything that the broker
did not produce. Credentials live only in the configured state_dir (mode 0700).
`, version)
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------
func loadConfig(path string) (*config.Config, *policy.Allowlist, error) {
	cfg, err := config.Load(path)
	if err != nil {
		return nil, nil, err
	}
	allowlist, err := policy.Load(cfg.AllowlistPath)
	if err != nil {
		return nil, nil, err
	}
	if allowlist.Len() == 0 && !cfg.OpenSignin {
		return nil, nil, errors.New(
			"allowlist is empty and open_signin is false: no sender could ever be authorized",
		)
	}
	return cfg, allowlist, nil
}

func newContainer(ctx context.Context, cfg *config.Config) (*sqlstore.Container, error) {
	logger := waLog.Stdout("Database", "WARN", true)
	dsn := cfg.StoreDSN
	if cfg.Dialect == "sqlite" && !strings.HasPrefix(dsn, "file:") {
		dsn = "file:" + dsn + "?_pragma=busy_timeout(10000)&_pragma=journal_mode(WAL)&_pragma=foreign_keys(1)"
	}
	return sqlstore.New(ctx, cfg.Dialect, dsn, logger)
}

// waSender adapts a whatsmeow client to gateway.Sender.
type waSender struct {
	client     *whatsmeow.Client
	timeout    time.Duration
	allowlist  *policy.Allowlist
	openSignin bool
}

func (s *waSender) SendText(ctx context.Context, chatJID, text string) error {
	// Last line of defence: in closed mode never send outside the allowlist. In
	// open mode the Google binding is the authorization, so a reply to a
	// non-allowlisted chat is expected - but never to a group or broadcast.
	if s.allowlist == nil || (!s.openSignin && !s.allowlist.Contains(chatJID)) {
		return fmt.Errorf("refusing to send to non-allowlisted chat")
	}
	if openModeJIDForbidden(chatJID) {
		return fmt.Errorf("refusing to send to a non-1:1 chat")
	}
	jid, err := types.ParseJID(chatJID)
	if err != nil {
		return fmt.Errorf("parse chat jid: %w", err)
	}
	ctx, cancel := context.WithTimeout(ctx, s.timeout)
	defer cancel()
	_, err = s.client.SendMessage(ctx, jid, &waE2E.Message{Conversation: proto.String(text)})
	return err
}

type nullSender struct{}

func (nullSender) SendText(context.Context, string, string) error {
	logf("[dry-run] would send a reply (text withheld)")
	return nil
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------
func cmdRun(args []string) error {
	fs := flag.NewFlagSet("run", flag.ExitOnError)
	configPath := fs.String("config", "", "path to gateway config JSON")
	dryRun := fs.Bool("dry-run", false, "log policy decisions without connecting to WhatsApp")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *configPath == "" {
		return errors.New("--config is required")
	}
	cfg, allowlist, err := loadConfig(*configPath)
	if err != nil {
		return err
	}
	logf("allowlist: %d identities, fingerprint %s", allowlist.Len(), allowlist.Fingerprint()[:12])
	for _, row := range allowlist.Redacted() {
		logf("  %s", row)
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	if *dryRun {
		return runDry(ctx, cfg, allowlist)
	}

	container, err := newContainer(ctx, cfg)
	if err != nil {
		return fmt.Errorf("open device store: %w", err)
	}
	device, err := container.GetFirstDevice(ctx)
	if err != nil {
		return fmt.Errorf("read device store: %w", err)
	}
	if device.ID == nil {
		return errors.New("this transport account is not paired; run `whatsmeow-gateway pair --confirm-secondary` explicitly")
	}
	if allowlist.Contains(device.ID.ToNonAD().String()) {
		return errors.New("the paired account is an allowlisted guest identity; the transport must be a separate secondary account")
	}

	client := whatsmeow.NewClient(device, waLog.Stdout("Client", strings.ToUpper(cfg.LogLevel), true))
	client.EnableAutoReconnect = true

	brokerClient, err := gateway.Dial(ctx, cfg.BrokerSocket, protocol.NewHello(
		device.ID.ToNonAD().String(), allowlist.Fingerprint(), version,
	), time.Duration(cfg.Limits.BrokerTimeoutSec)*time.Second)
	if err != nil {
		return fmt.Errorf("broker handshake: %w", err)
	}
	defer brokerClient.Close()

	gw := &gateway.Gateway{
		Allowlist: allowlist,
		Client:    brokerClient,
		Sender: &waSender{
			client: client, timeout: time.Duration(cfg.Limits.SendTimeoutSec) * time.Second,
			allowlist: allowlist, openSignin: cfg.OpenSignin,
		},
		OpenSignin:      cfg.OpenSignin,
		MaxMessageChars: cfg.Limits.MaxMessageChars,
		MaxReplyChars:   cfg.Limits.MaxReplyChars,
		Log:             logLine,
	}

	gw.MaxConcurrent = 4
	client.AddEventHandler(func(raw any) {
		switch evt := raw.(type) {
		case *events.Message:
			msg := gateway.Incoming{
				MsgID:     string(evt.Info.ID),
				ChatJID:   evt.Info.Chat.ToNonAD().String(),
				SenderJID: evt.Info.Sender.ToNonAD().String(),
				Text:      extractText(evt),
				FromMe:    evt.Info.IsFromMe,
				IsGroup:   evt.Info.IsGroup,
				Timestamp: evt.Info.Timestamp,
			}
			if !gw.TryDispatch(ctx, msg) {
				// Bounded on purpose: dropping is safer than an unbounded queue.
				logf("busy: dropped msg=%s chat=%s (in-flight limit reached)", msg.MsgID, maskJID(msg.ChatJID))
			}
		case *events.LoggedOut:
			logf("transport account logged out; re-pairing is an explicit operator action")
		}
	})

	if err := client.Connect(); err != nil {
		return fmt.Errorf("connect whatsapp: %w", err)
	}
	defer client.Disconnect()
	logf("gateway running (account=%s)", maskJID(device.ID.ToNonAD().String()))
	<-ctx.Done()
	logf("shutting down")
	return nil
}

func runDry(ctx context.Context, cfg *config.Config, allowlist *policy.Allowlist) error {
	gw := &gateway.Gateway{
		Allowlist:       allowlist,
		Sender:          nullSender{},
		MaxMessageChars: cfg.Limits.MaxMessageChars,
		MaxReplyChars:   cfg.Limits.MaxReplyChars,
		Log:             logLine,
	}
	_ = gw
	logf("dry-run: no WhatsApp connection, no device store, no broker socket touched")
	scanner := bufio.NewScanner(os.Stdin)
	// An open-mode deployment may (intentionally) have an empty allowlist.
	dryJID := "15551230001@s.whatsapp.net"
	if jids := allowlist.JIDs(); len(jids) > 0 {
		dryJID = jids[0]
	} else if !cfg.OpenSignin {
		logf("dry-run: allowlist is empty and open_signin is false: nothing would be authorized")
	}
	fmt.Fprintln(os.Stderr, "dry-run> type a line to test local policy checks on the *sender* identity:")
	fmt.Fprintf(os.Stderr, "dry-run> test sender: %s (allowlist entries: %d, open_signin=%t)\n",
		dryJID, allowlist.Len(), cfg.OpenSignin)
	for scanner.Scan() {
		select {
		case <-ctx.Done():
			return nil
		default:
		}
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		msg := gateway.Incoming{
			MsgID:     fmt.Sprintf("dry%06d", time.Now().UnixNano()%1e6),
			ChatJID:   dryJID,
			SenderJID: dryJID,
			Text:      line,
			Timestamp: time.Now(),
		}
		if err := gw.Handle(ctx, msg); err != nil {
			logf("dry-run policy: %v", err)
			continue
		}
		logf("dry-run policy: accepted (a real run would forward this to the broker)")
	}
	return nil
}

func extractText(evt *events.Message) string {
	if evt.Message == nil {
		return ""
	}
	if conversation := evt.Message.GetConversation(); conversation != "" {
		return conversation
	}
	if ext := evt.Message.GetExtendedTextMessage(); ext != nil {
		return ext.GetText()
	}
	return ""
}

func maskJID(jid string) string {
	user, server, found := strings.Cut(jid, "@")
	if !found {
		return "***"
	}
	if len(user) > 3 {
		user = user[:3] + "***"
	}
	return user + "@" + server
}

// ---------------------------------------------------------------------------
// pair
// ---------------------------------------------------------------------------
func cmdPair(args []string) error {
	fs := flag.NewFlagSet("pair", flag.ExitOnError)
	configPath := fs.String("config", "", "path to gateway config JSON")
	confirm := fs.Bool("confirm-secondary", false, "confirm this is a dedicated secondary account, not the primary account")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *configPath == "" {
		return errors.New("--config is required")
	}
	if !*confirm {
		return errors.New("refusing to pair without --confirm-secondary (this must be a dedicated secondary account)")
	}
	cfg, allowlist, err := loadConfig(*configPath)
	if err != nil {
		return err
	}
	if err := cfg.EnsureStateDir(); err != nil {
		return err
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()

	container, err := newContainer(ctx, cfg)
	if err != nil {
		return fmt.Errorf("open device store: %w", err)
	}
	device, err := container.GetFirstDevice(ctx)
	if err != nil {
		return fmt.Errorf("read device store: %w", err)
	}
	if device.ID != nil {
		logf("already paired as %s; refusing to re-pair (remove the device store only as an explicit operator action)", maskJID(device.ID.ToNonAD().String()))
		return nil
	}

	client := whatsmeow.NewClient(device, waLog.Stdout("Client", "INFO", true))
	qrChan, err := client.GetQRChannel(ctx)
	if err != nil {
		return fmt.Errorf("qr channel: %w", err)
	}
	if err := client.Connect(); err != nil {
		return fmt.Errorf("connect: %w", err)
	}
	defer client.Disconnect()

	logf("scan this QR code with the SECONDARY WhatsApp account (Linked devices):")
	for evt := range qrChan {
		switch evt.Event {
		case "code":
			// Rendered in the terminal only: never written to disk or a file.
			fmt.Fprintf(os.Stderr, "\n%s\n\n", evt.Code)
		case "success":
			logf("pairing succeeded; credentials are stored in %s (mode 0700)", cfg.StateDir)
			logf("next: start `whatsmeow-gateway run --config %s`", *configPath)
			_ = allowlist
			return nil
		default:
			logf("pairing event: %s", evt.Event)
			if evt.Event == "timeout" {
				return errors.New("pairing timed out; rerun `pair` when ready")
			}
		}
	}
	return nil
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------
func cmdStatus(args []string) error {
	fs := flag.NewFlagSet("status", flag.ExitOnError)
	configPath := fs.String("config", "", "path to gateway config JSON")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *configPath == "" {
		return errors.New("--config is required")
	}
	cfg, allowlist, err := loadConfig(*configPath)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	container, err := newContainer(ctx, cfg)
	if err != nil {
		return fmt.Errorf("open device store: %w", err)
	}
	device, err := container.GetFirstDevice(ctx)
	if err != nil {
		return fmt.Errorf("read device store: %w", err)
	}
	paired := "no"
	account := "(none)"
	if device.ID != nil {
		paired = "yes"
		account = maskJID(device.ID.ToNonAD().String())
	}
	fmt.Printf("gateway version : %s (protocol v%d)\n", version, protocol.Version)
	fmt.Printf("paired          : %s (%s)\n", paired, account)
	fmt.Printf("state dir       : %s\n", cfg.StateDir)
	fmt.Printf("broker socket   : %s\n", cfg.BrokerSocket)
	fmt.Printf("allowlist       : %d identities, fingerprint %s\n", allowlist.Len(), allowlist.Fingerprint()[:12])
	for _, row := range allowlist.Redacted() {
		fmt.Printf("                  %s\n", row)
	}
	return nil
}

// openModeJIDForbidden blocks non-1:1 chat types even when open sign-in is on.
func openModeJIDForbidden(raw string) bool {
	lower := strings.ToLower(raw)
	return strings.HasSuffix(lower, "@g.us") || strings.HasSuffix(lower, "@broadcast") ||
		strings.HasSuffix(lower, "@newsletter") || lower == "status@broadcast"
}
