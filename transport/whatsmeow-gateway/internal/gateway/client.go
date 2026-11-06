// Package gateway connects the WhatsApp transport to the trusted broker and
// applies the transport-side half of the security policy.
//
// Division of responsibility:
//   - the *broker* decides who a person is and which session answers;
//   - the *gateway* decides which WhatsApp chats are even worth forwarding.
//
// The gateway is treated as untrusted by the broker, so it also enforces a
// strict local policy: default deny, one-to-one chats only, no self-chats, no
// group chats, bounded message sizes, and no reply to a chat that is not the
// allowlisted identity's own chat.
package gateway

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"net"
	"strings"
	"sync"
	"time"

	"github.com/omnigent/pilot-wa-gateway/internal/policy"
	"github.com/omnigent/pilot-wa-gateway/internal/protocol"
)

// Sender is the outbound WhatsApp capability the gateway needs. The real
// implementation wraps a whatsmeow client; tests use a recorder.
type Sender interface {
	SendText(ctx context.Context, chatJID, text string) error
}

// Incoming is a transport-neutral view of one WhatsApp message.
type Incoming struct {
	MsgID     string
	ChatJID   string
	SenderJID string
	Text      string
	FromMe    bool
	IsGroup   bool
	Timestamp time.Time
}

// Client is a connection to the broker's Unix socket.
type Client struct {
	conn     net.Conn
	reader   *bufio.Reader
	writeMu  sync.Mutex
	deadline time.Duration
	Log      func(string)
}

// Dial connects, performs the hello handshake and returns a ready client.
func Dial(ctx context.Context, socketPath string, hello protocol.Hello, timeout time.Duration) (*Client, error) {
	var dialer net.Dialer
	conn, err := dialer.DialContext(ctx, "unix", socketPath)
	if err != nil {
		return nil, fmt.Errorf("connect broker socket %s: %w", socketPath, err)
	}
	client := &Client{conn: conn, reader: bufio.NewReader(conn), deadline: timeout, Log: func(string) {}}
	if err := client.handshake(hello); err != nil {
		conn.Close()
		return nil, err
	}
	return client, nil
}

func (c *Client) handshake(hello protocol.Hello) error {
	line, err := protocol.Encode(hello)
	if err != nil {
		return err
	}
	if err := c.write(line); err != nil {
		return err
	}
	_ = c.conn.SetReadDeadline(time.Now().Add(c.deadline))
	raw, err := protocol.ReadFrame(c.reader)
	if err != nil {
		return fmt.Errorf("handshake: %w", err)
	}
	frame, err := protocol.Decode(raw)
	if err != nil {
		return fmt.Errorf("handshake: %w", err)
	}
	switch typed := frame.(type) {
	case *protocol.HelloOK:
		if !typed.OK {
			return errors.New("broker rejected the handshake")
		}
		return nil
	case *protocol.Error:
		return fmt.Errorf("broker rejected the handshake: %s (%s)", typed.Detail, typed.Code)
	default:
		return fmt.Errorf("handshake: unexpected frame %T", frame)
	}
}

func (c *Client) write(line []byte) error {
	c.writeMu.Lock()
	defer c.writeMu.Unlock()
	_ = c.conn.SetWriteDeadline(time.Now().Add(c.deadline))
	_, err := c.conn.Write(line)
	return err
}

// Exchange sends one inbound message and waits for the ack and (optionally) the
// reply. The broker sends ack first, then a reply when it produced text.
func (c *Client) Exchange(inbound protocol.Inbound) (*protocol.Ack, *protocol.Reply, error) {
	line, err := protocol.EncodeInbound(inbound)
	if err != nil {
		return nil, nil, err
	}
	if err := c.write(line); err != nil {
		return nil, nil, err
	}
	var ack *protocol.Ack
	for {
		_ = c.conn.SetReadDeadline(time.Now().Add(c.deadline))
		raw, err := protocol.ReadFrame(c.reader)
		if err != nil {
			return ack, nil, err
		}
		frame, err := protocol.Decode(raw)
		if err != nil {
			return ack, nil, err
		}
		switch typed := frame.(type) {
		case *protocol.Ack:
			ack = typed
			if typed.MsgID != inbound.MsgID {
				return ack, nil, fmt.Errorf("ack for %q while waiting for %q", typed.MsgID, inbound.MsgID)
			}
			if typed.Status != "accepted" && typed.Status != "rate-limited" {
				return ack, nil, &DeniedError{Status: typed.Status}
			}
		case *protocol.Reply:
			if typed.MsgID != inbound.MsgID {
				return ack, nil, fmt.Errorf("reply for %q while waiting for %q", typed.MsgID, inbound.MsgID)
			}
			if typed.WAJID != inbound.WAJID {
				// The broker must answer the chat it was asked about.
				return ack, nil, errors.New("reply wa_jid does not match the inbound chat")
			}
			return ack, typed, nil
		case *protocol.Error:
			return ack, nil, fmt.Errorf("broker error %s: %s", typed.Code, typed.Detail)
		case *protocol.HelloOK:
			continue
		default:
			return ack, nil, fmt.Errorf("unexpected frame %T", frame)
		}
	}
}

// DeniedError means the broker refused to answer (rate limit, denied, failed).
type DeniedError struct{ Status string }

func (e *DeniedError) Error() string { return "broker status: " + e.Status }

// Close closes the socket.
func (c *Client) Close() error { return c.conn.Close() }

// Gateway applies local policy and bridges WhatsApp to the broker.
type Gateway struct {
	Allowlist     *policy.Allowlist
	Client        *Client
	Sender        Sender
	MaxMessageChars int
	MaxReplyChars   int
	Log             func(string)
}

// Handle validates one message and, if allowed, asks the broker and sends the
// answer back to the *sender's own chat*.
func (g *Gateway) Handle(ctx context.Context, msg Incoming) error {
	if g.Allowlist == nil || g.Allowlist.Len() == 0 {
		return policy.ErrDenied
	}
	if msg.FromMe {
		return errors.New("ignoring message sent by the transport account itself")
	}
	if msg.IsGroup {
		return errors.New("ignoring group chat")
	}
	if strings.TrimSpace(msg.Text) == "" {
		return errors.New("ignoring empty message")
	}
	if len([]rune(msg.Text)) > g.MaxMessageChars {
		return fmt.Errorf("message is %d characters (limit %d)", len([]rune(msg.Text)), g.MaxMessageChars)
	}
	userID, err := g.Allowlist.Resolve(msg.SenderJID)
	if err != nil {
		return err
	}
	// The chat must be the sender's own 1:1 chat: never answer on behalf of a
	// chat the allowlisted identity does not own.
	chatJID, err := policy.NormalizeJID(msg.ChatJID)
	if err != nil {
		return err
	}
	senderJID, err := policy.NormalizeJID(msg.SenderJID)
	if err != nil {
		return err
	}
	if chatJID != senderJID {
		return fmt.Errorf("chat %s is not the sender's own chat", maskJID(chatJID))
	}
	g.Log(fmt.Sprintf("inbound msg=%s from=%s user=%s chars=%d", msg.MsgID, maskJID(senderJID), userID, len([]rune(msg.Text))))

	if g.Client == nil {
		// Dry-run / policy-only mode: validated, but nothing is forwarded.
		g.Log(fmt.Sprintf("dry-run: msg=%s accepted by local policy, not forwarded", msg.MsgID))
		return nil
	}

	_, reply, err := g.Client.Exchange(protocol.NewInbound(msg.MsgID, senderJID, msg.Text, float64(msg.Timestamp.Unix())))
	if err != nil {
		return err
	}
	if reply == nil {
		return nil // broker accepted but produced no text (e.g. denial)
	}
	text := truncate(reply.Text, g.MaxReplyChars)
	if strings.TrimSpace(text) == "" {
		return nil
	}
	if err := g.Sender.SendText(ctx, chatJID, text); err != nil {
		return fmt.Errorf("send reply: %w", err)
	}
	g.Log(fmt.Sprintf("reply msg=%s chars=%d", msg.MsgID, len([]rune(text))))
	return nil
}

func truncate(text string, limit int) string {
	if limit <= 0 {
		return text
	}
	runes := []rune(text)
	if len(runes) <= limit {
		return text
	}
	return strings.TrimSpace(string(runes[:limit-1])) + "…"
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
