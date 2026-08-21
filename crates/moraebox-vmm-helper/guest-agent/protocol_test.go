package main

import (
	"bytes"
	"encoding/hex"
	"errors"
	"testing"
)

func TestFrameGoldenVector(t *testing.T) {
	frame := encodeFrame(wireFrame{
		version:   1,
		sessionID: "session",
		streamID:  1,
		sequence:  0,
		payload:   payloadStdinEOF,
		body:      nil,
	})
	const expected = "0801120773657373696f6e18016a00"
	if hex.EncodeToString(frame) != expected {
		t.Fatalf("frame = %x, want %s", frame, expected)
	}
	decoded, err := decodeFrame(frame)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.sessionID != "session" || decoded.payload != payloadStdinEOF {
		t.Fatalf("unexpected decoded frame: %#v", decoded)
	}
}

func TestReadFrameRejectsOversizeBeforeBody(t *testing.T) {
	header := []byte{0, 128, 0, 1}
	_, err := readFrame(bytes.NewReader(header))
	if !errors.Is(err, errFrameTooLarge) {
		t.Fatalf("error = %v, want frame-too-large", err)
	}
}

func TestDecodeExecPreservesArgvAndEnvironment(t *testing.T) {
	var body []byte
	body = appendBytesField(body, 1, []byte("/bin/echo"))
	body = appendBytesField(body, 1, []byte("hello world"))
	body = appendBytesField(body, 2, []byte("/workspace"))
	body = appendBytesField(body, 3, []byte("A=b c"))
	body = appendVarintField(body, 4, 1)
	body = appendVarintField(body, 5, 24)
	body = appendVarintField(body, 6, 80)
	request, err := decodeExec(body)
	if err != nil {
		t.Fatal(err)
	}
	if len(request.argv) != 2 || request.argv[1] != "hello world" || request.env[0] != "A=b c" {
		t.Fatalf("exec request was not preserved: %#v", request)
	}
}

func TestDecodeRejectsDuplicatePayload(t *testing.T) {
	frame := encodeFrame(wireFrame{
		version: 1, sessionID: "session", streamID: 1, payload: payloadStdinEOF,
	})
	frame = appendBytesField(frame, payloadShutdown, encodeShutdown("bad"))
	if _, err := decodeFrame(frame); err == nil {
		t.Fatal("duplicate payload was accepted")
	}
}

func TestCopyOutRequestRoundTrip(t *testing.T) {
	body := encodeCopyOutRequest(7, "/workspace/result", 4096)
	const expected = "080712112f776f726b73706163652f726573756c74188020"
	if hex.EncodeToString(body) != expected {
		t.Fatalf("copy-out request = %x, want %s", body, expected)
	}
	request, err := decodeCopyOutRequest(body)
	if err != nil {
		t.Fatal(err)
	}
	if request.transferID != 7 || request.source != "/workspace/result" || request.maxBytes != 4096 {
		t.Fatalf("unexpected copy-out request: %#v", request)
	}
}
