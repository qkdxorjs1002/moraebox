//go:build linux

package main

import (
	"bytes"
	"strings"
	"testing"
	"time"
)

func TestShortLivedProcessOutputIsDrainedBeforeWaitReturns(t *testing.T) {
	for iteration := 0; iteration < 64; iteration++ {
		var transport bytes.Buffer
		writer := &frameWriter{w: &transport, sessionID: "output-drain"}
		process, err := startProcess(execRequest{
			argv: []string{"/bin/sh", "-c", "printf ready; printf error >&2"},
			cwd:  "/",
			env:  []string{"PATH=" + defaultGuestPath},
		}, writer)
		if err != nil {
			t.Fatalf("iteration %d: start process: %v", iteration, err)
		}
		result := process.wait()
		if result.code != 0 || result.signal != nil {
			t.Fatalf("iteration %d: result = %+v", iteration, result)
		}

		var stdout, stderr []byte
		for transport.Len() > 0 {
			frame, err := readFrame(&transport)
			if err != nil {
				t.Fatalf("iteration %d: decode output frame: %v", iteration, err)
			}
			if frame.payload != payloadOutput {
				t.Fatalf("iteration %d: unexpected payload %d", iteration, frame.payload)
			}
			channel, data, err := decodeOutput(frame.body)
			if err != nil {
				t.Fatalf("iteration %d: decode output: %v", iteration, err)
			}
			switch channel {
			case 0:
				stdout = append(stdout, data...)
			case 1:
				stderr = append(stderr, data...)
			default:
				t.Fatalf("iteration %d: unexpected output channel %d", iteration, channel)
			}
		}
		if string(stdout) != "ready" || string(stderr) != "error" {
			t.Fatalf("iteration %d: stdout=%q stderr=%q", iteration, stdout, stderr)
		}
	}
}

func TestTTYCloseStdinDeliversEOF(t *testing.T) {
	for _, test := range []struct {
		name  string
		input string
	}{
		{name: "complete-line", input: "complete line\n"},
		{name: "partial-line", input: "partial line"},
	} {
		t.Run(test.name, func(t *testing.T) {
			input := test.input
			var transport bytes.Buffer
			writer := &frameWriter{w: &transport, sessionID: "tty-eof"}
			process, err := startProcess(execRequest{
				argv: []string{"/bin/cat"},
				cwd:  "/",
				env:  []string{"PATH=" + defaultGuestPath},
				tty:  true,
				rows: 24,
				cols: 80,
			}, writer)
			if err != nil {
				t.Fatalf("start terminal process: %v", err)
			}
			if err := process.writeStdin([]byte(input)); err != nil {
				t.Fatalf("write terminal stdin: %v", err)
			}
			if err := process.closeStdin(); err != nil {
				t.Fatalf("close terminal stdin: %v", err)
			}

			result := make(chan processResult, 1)
			go func() { result <- process.wait() }()
			select {
			case status := <-result:
				if status.code != 0 || status.signal != nil {
					t.Fatalf("terminal process result = %+v", status)
				}
			case <-time.After(2 * time.Second):
				_ = process.kill(9)
				<-result
				t.Fatal("terminal process did not exit after stdin EOF")
			}

			var output []byte
			for transport.Len() > 0 {
				frame, err := readFrame(&transport)
				if err != nil {
					t.Fatalf("decode terminal output frame: %v", err)
				}
				channel, data, err := decodeOutput(frame.body)
				if err != nil {
					t.Fatalf("decode terminal output: %v", err)
				}
				if channel != 2 {
					t.Fatalf("terminal output channel = %d", channel)
				}
				output = append(output, data...)
			}
			if !bytes.Contains(output, []byte(strings.TrimSpace(input))) {
				t.Fatalf("terminal output %q does not contain input %q", output, input)
			}
		})
	}
}

func decodeOutput(input []byte) (uint64, []byte, error) {
	var channel uint64
	var data []byte
	fields := wireFields{input: input}
	for fields.more() {
		field, wire, value, raw, err := fields.next()
		if err != nil {
			return 0, nil, err
		}
		switch field {
		case 1:
			if wire != 0 {
				return 0, nil, errMalformedProtobuf
			}
			channel = value
		case 2:
			if wire != 2 {
				return 0, nil, errMalformedProtobuf
			}
			data = append([]byte(nil), raw...)
		}
	}
	return channel, data, nil
}
