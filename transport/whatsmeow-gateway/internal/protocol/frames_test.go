package protocol

import (
	"fmt"
	"strings"
	"testing"
)

var (
	versionJSON      = fmt.Sprint(Version)
	wrongVersionJSON = fmt.Sprint(Version + 1)
)

func TestDecodeRejectsForbiddenSelectionFields(t *testing.T) {
	for _, field := range []string{"session_id", "user_id", "model", "workspace", "command"} {
		line := `{"v":` + versionJSON + `,"type":"ack","msg_id":"m1","status":"accepted","` + field + `":"x"}`
		if _, err := Decode([]byte(line)); err == nil {
			t.Fatalf("field %q must be rejected", field)
		}
	}
}

func TestDecodeRejectsUnknownFields(t *testing.T) {
	line := `{"v":` + versionJSON + `,"type":"ack","msg_id":"m1","status":"accepted","extra":1}`
	if _, err := Decode([]byte(line)); err == nil {
		t.Fatal("unknown field must be rejected")
	}
}

func TestDecodeRejectsWrongVersion(t *testing.T) {
	if _, err := Decode([]byte(`{"v":` + wrongVersionJSON + `,"type":"ack","msg_id":"m1","status":"accepted"}`)); err == nil {
		t.Fatal("unsupported version must be rejected")
	}
}

func TestDecodeRejectsOversizeFrame(t *testing.T) {
	big := strings.Repeat("x", MaxFrameBytes+1)
	if _, err := Decode([]byte(`{"v":` + versionJSON + `,"type":"ack","msg_id":"` + big + `","status":"accepted"}`)); err == nil {
		t.Fatal("oversized frame must be rejected")
	}
}

func TestDecodeReplyAndAck(t *testing.T) {
	frame, err := Decode([]byte(`{"v":` + versionJSON + `,"type":"reply","msg_id":"m1","wa_jid":"15551230001@s.whatsapp.net","text":"hi","ts":1}`))
	if err != nil {
		t.Fatal(err)
	}
	reply, ok := frame.(*Reply)
	if !ok || reply.Text != "hi" {
		t.Fatalf("unexpected frame %#v", frame)
	}
	frame, err = Decode([]byte(`{"v":` + versionJSON + `,"type":"ack","msg_id":"m1","status":"denied"}`))
	if err != nil {
		t.Fatal(err)
	}
	if ack, ok := frame.(*Ack); !ok || ack.Status != "denied" {
		t.Fatalf("unexpected frame %#v", frame)
	}
}

func TestEncodeInboundHasRequiredFieldsOnly(t *testing.T) {
	line, err := EncodeInbound(NewInbound("m1", "15551230001@s.whatsapp.net", "hello", 1700000000))
	if err != nil {
		t.Fatal(err)
	}
	text := string(line)
	for _, want := range []string{`"type":"inbound"`, `"msg_id":"m1"`, `"wa_jid"`, `"text":"hello"`} {
		if !strings.Contains(text, want) {
			t.Fatalf("encoded frame missing %s: %s", want, text)
		}
	}
	for _, forbiddenKey := range forbidden {
		if strings.Contains(text, `"`+forbiddenKey+`"`) {
			t.Fatalf("encoded frame contains forbidden field %q", forbiddenKey)
		}
	}
	if !strings.HasSuffix(text, "\n") {
		t.Fatal("frame must be newline terminated")
	}
}
