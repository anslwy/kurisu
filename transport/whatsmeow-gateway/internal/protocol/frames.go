// Package protocol implements the NDJSON frames exchanged with the broker.
//
// Compatibility contract with broker/guest_broker/protocol.py:
//   - one JSON object per line, UTF-8, at most 64 KiB per frame;
//   - strict decoding: unknown or forbidden fields are errors, not warnings;
//   - the gateway never sends a session id, model, workspace or command.
package protocol

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

// Version is the only supported wire version.
const Version = 1

// MaxFrameBytes bounds a single frame in both directions.
const MaxFrameBytes = 64 * 1024

// Message types.
const (
	TypeHello   = "hello"
	TypeInbound = "inbound"
	TypeReply   = "reply"
	TypeAck     = "ack"
	TypeError   = "error"
)

// forbidden lists fields the gateway must never send or accept. Their presence
// means the peer is trying to steer session choice, which is a hard failure.
var forbidden = []string{
	"session_id", "session", "user_id", "user", "model", "provider",
	"workspace", "cwd", "command", "argv", "env", "limits",
}

// Hello is the first frame the gateway sends.
type Hello struct {
	V              int    `json:"v"`
	Type           string `json:"type"`
	AccountJID     string `json:"account_jid"`
	AllowlistSHA   string `json:"allowlist_sha256"`
	GatewayVersion string `json:"gateway_version"`
}

// HelloOK is the broker's acceptance frame.
type HelloOK struct {
	V             int    `json:"v"`
	Type          string `json:"type"`
	OK            bool   `json:"ok"`
	BrokerVersion string `json:"broker_version"`
}

// Inbound is a message the guest should answer.
type Inbound struct {
	V     int     `json:"v"`
	Type  string  `json:"type"`
	MsgID string  `json:"msg_id"`
	WAJID string  `json:"wa_jid"`
	Text  string  `json:"text"`
	TS    float64 `json:"ts"`
}

// Reply is the broker's answer for one inbound message.
type Reply struct {
	V     int     `json:"v"`
	Type  string  `json:"type"`
	MsgID string  `json:"msg_id"`
	WAJID string  `json:"wa_jid"`
	Text  string  `json:"text"`
	TS    float64 `json:"ts"`
}

// Ack reports whether the broker accepted the message.
type Ack struct {
	V      int    `json:"v"`
	Type   string `json:"type"`
	MsgID  string `json:"msg_id"`
	Status string `json:"status"`
}

// Error is a broker-side rejection.
type Error struct {
	V      int    `json:"v"`
	Type   string `json:"type"`
	MsgID  string `json:"msg_id,omitempty"`
	Code   string `json:"code"`
	Detail string `json:"detail"`
}

// NewHello builds the handshake frame.
func NewHello(accountJID, allowlistSHA, version string) Hello {
	return Hello{V: Version, Type: TypeHello, AccountJID: accountJID, AllowlistSHA: allowlistSHA, GatewayVersion: version}
}

// NewInbound builds an inbound frame.
func NewInbound(msgID, waJID, text string, ts float64) Inbound {
	return Inbound{V: Version, Type: TypeInbound, MsgID: msgID, WAJID: waJID, Text: text, TS: ts}
}

func checkForbidden(raw map[string]json.RawMessage) error {
	for _, field := range forbidden {
		if _, ok := raw[field]; ok {
			return fmt.Errorf("forbidden field %q: the gateway may never carry session/model/workspace selection", field)
		}
	}
	return nil
}

func checkVersion(v int) error {
	if v != Version {
		return fmt.Errorf("unsupported protocol version %d", v)
	}
	return nil
}

// Decode parses one frame, rejecting unknown/forbidden fields and bad versions.
func Decode(line []byte) (any, error) {
	if len(line) > MaxFrameBytes {
		return nil, fmt.Errorf("frame too large (%d bytes > %d)", len(line), MaxFrameBytes)
	}
	var probe map[string]json.RawMessage
	if err := json.Unmarshal(line, &probe); err != nil {
		return nil, fmt.Errorf("frame is not a JSON object: %w", err)
	}
	if err := checkForbidden(probe); err != nil {
		return nil, err
	}
	var head struct {
		V    int    `json:"v"`
		Type string `json:"type"`
	}
	if err := json.Unmarshal(line, &head); err != nil {
		return nil, err
	}
	if err := checkVersion(head.V); err != nil {
		return nil, err
	}
	strict := func(target any) (any, error) {
		dec := json.NewDecoder(bytesReader(line))
		dec.DisallowUnknownFields()
		if err := dec.Decode(target); err != nil {
			return nil, fmt.Errorf("strict decode failed: %w", err)
		}
		return target, nil
	}
	switch head.Type {
	case TypeReply:
		var frame Reply
		decoded, err := strict(&frame)
		if err != nil {
			return nil, err
		}
		if frame.MsgID == "" || frame.WAJID == "" {
			return nil, errors.New("reply: msg_id and wa_jid are required")
		}
		return decoded, nil
	case TypeAck:
		var frame Ack
		decoded, err := strict(&frame)
		if err != nil {
			return nil, err
		}
		return decoded, nil
	case TypeError:
		var frame Error
		decoded, err := strict(&frame)
		if err != nil {
			return nil, err
		}
		return decoded, nil
	case TypeHello:
		var frame HelloOK
		decoded, err := strict(&frame)
		if err != nil {
			return nil, err
		}
		return decoded, nil
	case TypeInbound:
		return nil, errors.New("broker must not receive inbound frames from itself")
	default:
		return nil, fmt.Errorf("unknown frame type %q", head.Type)
	}
}

// Encode serializes a frame as a single NDJSON line.
func Encode(frame Hello) ([]byte, error)     { return marshalLine(frame) }
func EncodeInbound(f Inbound) ([]byte, error) { return marshalLine(f) }

func marshalLine(frame any) ([]byte, error) {
	data, err := json.Marshal(frame)
	if err != nil {
		return nil, err
	}
	if len(data)+1 > MaxFrameBytes {
		return nil, fmt.Errorf("outbound frame too large (%d bytes)", len(data)+1)
	}
	return append(data, '\n'), nil
}

// ReadFrame reads one line, bounded by MaxFrameBytes.
func ReadFrame(r *bufio.Reader) ([]byte, error) {
	line, err := r.ReadSlice('\n')
	if err == bufio.ErrBufferFull {
		// Drain up to the cap so we fail cleanly instead of desynchronising.
		total := len(line)
		for err == bufio.ErrBufferFull && total <= MaxFrameBytes {
			_, err = r.ReadSlice('\n')
			if err == nil {
				break
			}
			total += 4096
		}
		return nil, fmt.Errorf("frame exceeds %d bytes", MaxFrameBytes)
	}
	if err != nil && err != io.EOF {
		return nil, err
	}
	if len(line) > MaxFrameBytes {
		return nil, fmt.Errorf("frame exceeds %d bytes", MaxFrameBytes)
	}
	return line, nil
}

// bytesReader avoids importing bytes at every call site.
func bytesReader(b []byte) io.Reader { return &reader{b: b} }

type reader struct{ b []byte }

func (r *reader) Read(p []byte) (int, error) {
	if len(r.b) == 0 {
		return 0, io.EOF
	}
	n := copy(p, r.b)
	r.b = r.b[n:]
	return n, nil
}
