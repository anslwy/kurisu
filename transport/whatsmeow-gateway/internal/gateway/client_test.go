package gateway

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/omnigent/pilot-wa-gateway/internal/policy"
	"github.com/omnigent/pilot-wa-gateway/internal/protocol"
)

// fakeBroker is a minimal broker for tests: it records inbound frames and
// answers with a fixed policy so the gateway's own behaviour is exercised.
type fakeBroker struct {
	listener net.Listener
	path     string
	got      chan protocol.Inbound
	hello    chan protocol.Hello
	reply    string
	status   string
}

func newFakeBroker(t *testing.T) *fakeBroker {
	t.Helper()
	path := filepath.Join(t.TempDir(), "broker.sock")
	ln, err := net.Listen("unix", path)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	fb := &fakeBroker{listener: ln, path: path, got: make(chan protocol.Inbound, 256), hello: make(chan protocol.Hello, 2), reply: "answer from broker", status: "accepted"}
	t.Cleanup(func() { ln.Close() })
	go fb.serve()
	return fb
}

func (fb *fakeBroker) serve() {
	for {
		conn, err := fb.listener.Accept()
		if err != nil {
			return
		}
		go fb.handle(conn)
	}
}

func (fb *fakeBroker) handle(conn net.Conn) {
	defer conn.Close()
	reader := bufio.NewReader(conn)
	for {
		line, err := protocol.ReadFrame(reader)
		if err != nil {
			return
		}
		var head struct {
			Type string `json:"type"`
		}
		_ = json.Unmarshal(line, &head)
		if head.Type == protocol.TypeHello {
			var hello protocol.Hello
			_ = json.Unmarshal(line, &hello)
			fb.hello <- hello
			payload, _ := json.Marshal(map[string]any{"v": protocol.Version, "type": "hello", "ok": true, "broker_version": "test"})
			conn.Write(append(payload, '\n'))
			continue
		}
		var inbound protocol.Inbound
		if err := json.Unmarshal(line, &inbound); err != nil {
			return
		}
		fb.got <- inbound
		ack, _ := json.Marshal(protocol.Ack{V: protocol.Version, Type: "ack", MsgID: inbound.MsgID, Status: fb.status})
		conn.Write(append(ack, '\n'))
		// v2: a reply frame always follows the ack, possibly with empty text.
		reply, _ := json.Marshal(protocol.Reply{
			V: protocol.Version, Type: "reply", MsgID: inbound.MsgID,
			WAJID: inbound.WAJID, Text: fb.reply, TS: 1,
		})
		conn.Write(append(reply, '\n'))
	}
}

// newNeverBroker accepts the handshake and then never answers a message, so a
// handler blocks while the dispatch slot is occupied.
func newNeverBroker(t *testing.T) *fakeBroker {
	t.Helper()
	path := filepath.Join(t.TempDir(), "never.sock")
	ln, err := net.Listen("unix", path)
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { ln.Close() })
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			go func(c net.Conn) {
				defer c.Close()
				reader := bufio.NewReader(c)
				line, err := protocol.ReadFrame(reader)
				if err != nil {
					return
				}
				var head struct {
					Type string `json:"type"`
				}
				_ = json.Unmarshal(line, &head)
				if head.Type == protocol.TypeHello {
					payload, _ := json.Marshal(map[string]any{"v": protocol.Version, "type": "hello", "ok": true, "broker_version": "test"})
					c.Write(append(payload, '\n'))
				}
				// deliberately never answer further frames
				<-make(chan struct{})
			}(conn)
		}
	}()
	return &fakeBroker{listener: ln, path: path}
}

type recorder struct {
	sent []string
	err  error
}

func (r *recorder) SendText(_ context.Context, chatJID, text string) error {
	if r.err != nil {
		return r.err
	}
	r.sent = append(r.sent, chatJID+"|"+text)
	return nil
}

func testAllowlist(t *testing.T) *policy.Allowlist {
	t.Helper()
	a, err := policy.New([]policy.Identity{{WAJID: "15551230001@s.whatsapp.net", UserID: "pilot-001"}})
	if err != nil {
		t.Fatal(err)
	}
	return a
}

func newGateway(t *testing.T, fb *fakeBroker, sender Sender) *Gateway {
	t.Helper()
	allowlist := testAllowlist(t)
	client, err := Dial(context.Background(), fb.path, protocol.NewHello("15559990001@s.whatsapp.net", allowlist.Fingerprint(), "test"), 5*time.Second)
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { client.Close() })
	return &Gateway{
		Allowlist:       allowlist,
		Client:          client,
		Sender:          sender,
		MaxMessageChars: 100,
		MaxReplyChars:   50,
		Log:             func(string) {},
	}
}

func msgFor(jid, text string) Incoming {
	return Incoming{MsgID: "m1", ChatJID: jid, SenderJID: jid, Text: text, Timestamp: time.Unix(1700000000, 0)}
}

func TestHandleForwardsAllowlistedMessageAndSendsReply(t *testing.T) {
	fb := newFakeBroker(t)
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "hello")); err != nil {
		t.Fatalf("handle: %v", err)
	}
	select {
	case got := <-fb.got:
		if got.Text != "hello" || got.WAJID != "15551230001@s.whatsapp.net" {
			t.Fatalf("unexpected inbound %+v", got)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("broker did not receive the message")
	}
	if len(rec.sent) != 1 || !strings.HasPrefix(rec.sent[0], "15551230001@s.whatsapp.net|") {
		t.Fatalf("unexpected sends %v", rec.sent)
	}
}

func TestHandleDropsNonAllowlistedSender(t *testing.T) {
	fb := newFakeBroker(t)
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	if err := gw.Handle(context.Background(), msgFor("15550000000@s.whatsapp.net", "let me in")); err == nil {
		t.Fatal("non-allowlisted sender must be dropped")
	}
	select {
	case got := <-fb.got:
		t.Fatalf("broker must not receive denied messages, got %+v", got)
	case <-time.After(300 * time.Millisecond):
	}
	if len(rec.sent) != 0 {
		t.Fatalf("must not send anything: %v", rec.sent)
	}
}

func TestHandleDropsGroupSelfAndEmpty(t *testing.T) {
	fb := newFakeBroker(t)
	rec := &recorder{}
	gw := newGateway(t, fb, rec)

	group := msgFor("15551230001@s.whatsapp.net", "hi")
	group.IsGroup = true
	if err := gw.Handle(context.Background(), group); err == nil {
		t.Fatal("group message must be dropped")
	}
	self := msgFor("15551230001@s.whatsapp.net", "hi")
	self.FromMe = true
	if err := gw.Handle(context.Background(), self); err == nil {
		t.Fatal("fromMe message must be dropped")
	}
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "   ")); err == nil {
		t.Fatal("empty message must be dropped")
	}
	if len(rec.sent) != 0 {
		t.Fatalf("must not send anything: %v", rec.sent)
	}
}

func TestHandleDropsOverlongMessage(t *testing.T) {
	fb := newFakeBroker(t)
	gw := newGateway(t, fb, &recorder{})
	long := strings.Repeat("a", 101)
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", long)); err == nil {
		t.Fatal("overlong message must be dropped")
	}
}

func TestHandleRefusesChatThatIsNotTheSendersOwn(t *testing.T) {
	fb := newFakeBroker(t)
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	msg := msgFor("15551230001@s.whatsapp.net", "hi")
	msg.ChatJID = "15551239999@s.whatsapp.net"
	if err := gw.Handle(context.Background(), msg); err == nil {
		t.Fatal("must refuse to answer a chat the sender does not own")
	}
	if len(rec.sent) != 0 {
		t.Fatalf("must not send anything: %v", rec.sent)
	}
}

func TestHandleSendsTheBrokersTextForNonAcceptedStatuses(t *testing.T) {
	// The real broker answers rate-limited / busy / failed turns with a short
	// guest-facing sentence. The gateway must deliver it (not drop it), and must
	// not desynchronise the next exchange.
	for _, status := range []string{"rate-limited", "failed", "denied"} {
		fb := newFakeBroker(t)
		fb.status = status
		fb.reply = "please wait a moment"
		rec := &recorder{}
		gw := newGateway(t, fb, rec)
		if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "spam")); err != nil {
			t.Fatalf("%s: unexpected error: %v", status, err)
		}
		if len(rec.sent) != 1 || !strings.Contains(rec.sent[0], "please wait") {
			t.Fatalf("%s: expected the broker text to be sent, got %v", status, rec.sent)
		}
	}
}

func TestHandleDoesNotSendWhenBrokerDeniesWithoutText(t *testing.T) {
	fb := newFakeBroker(t)
	fb.status = "denied"
	fb.reply = ""
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "spam")); err != nil {
		t.Fatalf("an empty denial reply must not be an error: %v", err)
	}
	if len(rec.sent) != 0 {
		t.Fatalf("must not send anything: %v", rec.sent)
	}
}

func TestTryDispatchBoundsConcurrency(t *testing.T) {
	fb := newFakeBroker(t)
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	gw.MaxConcurrent = 1
	gw.EnsureDispatch()
	// occupy the single slot with a handler that blocks on a never-answering broker
	blocked := newNeverBroker(t)
	gw.Client, _ = Dial(context.Background(), blocked.path, protocol.NewHello("15559990001@s.whatsapp.net", testAllowlist(t).Fingerprint(), "test"), time.Second)
	defer gw.Client.Close()
	msg := msgFor("15551230001@s.whatsapp.net", "hi")
	if !gw.TryDispatch(context.Background(), msg) {
		t.Fatal("first dispatch should be accepted")
	}
	time.Sleep(50 * time.Millisecond)
	if gw.TryDispatch(context.Background(), msgFor("15551230001@s.whatsapp.net", "second")) {
		t.Fatal("second dispatch must be rejected while the slot is busy")
	}
}

func TestConcurrentExchangesDoNotDesynchronise(t *testing.T) {
	fb := newFakeBroker(t)
	fb.reply = "answer"
	rec := &recorder{}
	gw := newGateway(t, fb, rec)

	const n = 24
	var wg sync.WaitGroup
	errCh := make(chan error, n)
	for i := 0; i < n; i++ {
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			msg := msgFor("15551230001@s.whatsapp.net", "hi")
			msg.MsgID = fmt.Sprintf("msg%04d", i)
			if err := gw.Handle(context.Background(), msg); err != nil {
				errCh <- fmt.Errorf("msg %s: %w", msg.MsgID, err)
			}
		}(i)
	}
	wg.Wait()
	close(errCh)
	for err := range errCh {
		t.Fatalf("concurrent exchange failed: %v", err)
	}
	if len(rec.sent) != n {
		t.Fatalf("expected %d replies, got %d", n, len(rec.sent))
	}
}

func TestHandleTruncatesLongReplies(t *testing.T) {
	fb := newFakeBroker(t)
	fb.reply = strings.Repeat("z", 200)
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "hi")); err != nil {
		t.Fatal(err)
	}
	if len(rec.sent) != 1 {
		t.Fatalf("expected one send, got %v", rec.sent)
	}
	text := strings.SplitN(rec.sent[0], "|", 2)[1]
	if len([]rune(text)) > gw.MaxReplyChars {
		t.Fatalf("reply not truncated: %d runes", len([]rune(text)))
	}
}

func TestDialFailsWhenBrokerRejectsHandshake(t *testing.T) {
	path := filepath.Join(t.TempDir(), "bad.sock")
	ln, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		reader := bufio.NewReader(conn)
		_, _ = protocol.ReadFrame(reader)
		payload, _ := json.Marshal(map[string]any{"v": protocol.Version, "type": "error", "code": "denied", "detail": "allowlist fingerprint mismatch"})
		conn.Write(append(payload, '\n'))
	}()
	allowlist := testAllowlist(t)
	_, err = Dial(context.Background(), path, protocol.NewHello("15559990001@s.whatsapp.net", allowlist.Fingerprint(), "test"), 3*time.Second)
	if err == nil || !strings.Contains(err.Error(), "handshake") {
		t.Fatalf("expected handshake failure, got %v", err)
	}
}

func TestEmptyAllowlistDeniesEverything(t *testing.T) {
	empty, _ := policy.New(nil)
	gw := &Gateway{Allowlist: empty, MaxMessageChars: 100, MaxReplyChars: 100, Log: func(string) {}}
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "hi")); err == nil {
		t.Fatal("empty allowlist must deny")
	}
}

func TestEnsureDispatchIsRaceSafe(t *testing.T) {
	gw := &Gateway{MaxConcurrent: 3}
	var wg sync.WaitGroup
	for i := 0; i < 32; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			gw.EnsureDispatch()
		}()
	}
	wg.Wait()
	if gw.inflight == nil {
		t.Fatal("dispatch channel was never initialised")
	}
	if cap(gw.inflight) != 3 {
		t.Fatalf("expected a single bounded channel of capacity 3, got %d", cap(gw.inflight))
	}
}
