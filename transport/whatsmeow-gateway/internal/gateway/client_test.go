package gateway

import (
	"bufio"
	"context"
	"encoding/json"
	"net"
	"path/filepath"
	"strings"
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
	fb := &fakeBroker{listener: ln, path: path, got: make(chan protocol.Inbound, 8), hello: make(chan protocol.Hello, 2), reply: "answer from broker", status: "accepted"}
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
			payload, _ := json.Marshal(map[string]any{"v": 1, "type": "hello", "ok": true, "broker_version": "test"})
			conn.Write(append(payload, '\n'))
			continue
		}
		var inbound protocol.Inbound
		if err := json.Unmarshal(line, &inbound); err != nil {
			return
		}
		fb.got <- inbound
		ack, _ := json.Marshal(protocol.Ack{V: 1, Type: "ack", MsgID: inbound.MsgID, Status: fb.status})
		conn.Write(append(ack, '\n'))
		if fb.status == "accepted" && fb.reply != "" {
			reply, _ := json.Marshal(protocol.Reply{V: 1, Type: "reply", MsgID: inbound.MsgID, WAJID: inbound.WAJID, Text: fb.reply, TS: 1})
			conn.Write(append(reply, '\n'))
		}
	}
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

func TestHandleDoesNotSendWhenBrokerDenies(t *testing.T) {
	fb := newFakeBroker(t)
	fb.status = "rate-limited"
	rec := &recorder{}
	gw := newGateway(t, fb, rec)
	if err := gw.Handle(context.Background(), msgFor("15551230001@s.whatsapp.net", "spam")); err == nil {
		t.Fatal("expected a denial error")
	}
	if len(rec.sent) != 0 {
		t.Fatalf("must not send anything: %v", rec.sent)
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
		payload, _ := json.Marshal(map[string]any{"v": 1, "type": "error", "code": "denied", "detail": "allowlist fingerprint mismatch"})
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
