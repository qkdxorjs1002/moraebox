package main

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"sync"
)

const (
	protocolVersion = uint64(1)
	maxFrameSize    = 8 * 1024 * 1024
	execStreamID    = uint64(1)

	payloadHello    = 10
	payloadExec     = 11
	payloadStdin    = 12
	payloadStdinEOF = 13
	payloadResize   = 14
	payloadSignal   = 15
	payloadOutput   = 16
	payloadExit     = 17
	payloadShutdown = 18
)

var (
	errMalformedProtobuf = errors.New("malformed protobuf")
	errFrameTooLarge     = errors.New("frame exceeds the protocol limit")
)

type wireFrame struct {
	version   uint64
	sessionID string
	streamID  uint64
	sequence  uint64
	payload   int
	body      []byte
}

type execRequest struct {
	argv []string
	cwd  string
	env  []string
	tty  bool
	rows uint32
	cols uint32
}

type frameWriter struct {
	mu        sync.Mutex
	w         io.Writer
	sessionID string
	sequence  uint64
}

func (w *frameWriter) send(payload int, body []byte) error {
	w.mu.Lock()
	defer w.mu.Unlock()
	frame := encodeFrame(wireFrame{
		version:   protocolVersion,
		sessionID: w.sessionID,
		streamID:  execStreamID,
		sequence:  w.sequence,
		payload:   payload,
		body:      body,
	})
	if len(frame) > maxFrameSize {
		return errFrameTooLarge
	}
	w.sequence++
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(frame)))
	if err := writeAll(w.w, header[:]); err != nil {
		return err
	}
	return writeAll(w.w, frame)
}

func readFrame(r io.Reader) (wireFrame, error) {
	var header [4]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return wireFrame{}, fmt.Errorf("read frame header: %w", err)
	}
	size := binary.BigEndian.Uint32(header[:])
	if size > maxFrameSize {
		return wireFrame{}, errFrameTooLarge
	}
	body := make([]byte, int(size))
	if _, err := io.ReadFull(r, body); err != nil {
		return wireFrame{}, fmt.Errorf("read frame body: %w", err)
	}
	return decodeFrame(body)
}

func encodeFrame(frame wireFrame) []byte {
	var output []byte
	if frame.version != 0 {
		output = appendVarintField(output, 1, frame.version)
	}
	if frame.sessionID != "" {
		output = appendBytesField(output, 2, []byte(frame.sessionID))
	}
	if frame.streamID != 0 {
		output = appendVarintField(output, 3, frame.streamID)
	}
	if frame.sequence != 0 {
		output = appendVarintField(output, 4, frame.sequence)
	}
	output = appendBytesField(output, frame.payload, frame.body)
	return output
}

func decodeFrame(input []byte) (wireFrame, error) {
	var frame wireFrame
	fields := wireFields{input: input}
	for fields.more() {
		field, wire, value, raw, err := fields.next()
		if err != nil {
			return wireFrame{}, err
		}
		switch field {
		case 1:
			if wire != 0 {
				return wireFrame{}, errMalformedProtobuf
			}
			frame.version = value
		case 2:
			if wire != 2 {
				return wireFrame{}, errMalformedProtobuf
			}
			frame.sessionID = string(raw)
		case 3:
			if wire != 0 {
				return wireFrame{}, errMalformedProtobuf
			}
			frame.streamID = value
		case 4:
			if wire != 0 {
				return wireFrame{}, errMalformedProtobuf
			}
			frame.sequence = value
		case payloadHello, payloadExec, payloadStdin, payloadStdinEOF, payloadResize,
			payloadSignal, payloadOutput, payloadExit, payloadShutdown:
			if wire != 2 || frame.payload != 0 {
				return wireFrame{}, errMalformedProtobuf
			}
			frame.payload = field
			frame.body = append([]byte(nil), raw...)
		}
	}
	if frame.version != protocolVersion || frame.sessionID == "" || frame.payload == 0 {
		return wireFrame{}, errMalformedProtobuf
	}
	return frame, nil
}

func decodeExec(input []byte) (execRequest, error) {
	var request execRequest
	fields := wireFields{input: input}
	for fields.more() {
		field, wire, value, raw, err := fields.next()
		if err != nil {
			return execRequest{}, err
		}
		switch field {
		case 1:
			if wire != 2 {
				return execRequest{}, errMalformedProtobuf
			}
			request.argv = append(request.argv, string(raw))
		case 2:
			if wire != 2 {
				return execRequest{}, errMalformedProtobuf
			}
			request.cwd = string(raw)
		case 3:
			if wire != 2 {
				return execRequest{}, errMalformedProtobuf
			}
			request.env = append(request.env, string(raw))
		case 4:
			if wire != 0 {
				return execRequest{}, errMalformedProtobuf
			}
			request.tty = value != 0
		case 5:
			if wire != 0 {
				return execRequest{}, errMalformedProtobuf
			}
			request.rows = uint32(value)
		case 6:
			if wire != 0 {
				return execRequest{}, errMalformedProtobuf
			}
			request.cols = uint32(value)
		}
	}
	if len(request.argv) == 0 || request.argv[0] == "" {
		return execRequest{}, errors.New("exec argv is empty")
	}
	if len(request.argv) > 4096 || len(request.env) > 4096 {
		return execRequest{}, errors.New("exec argument count exceeds the agent limit")
	}
	return request, nil
}

func decodeBytesField(input []byte, expected int) ([]byte, error) {
	fields := wireFields{input: input}
	for fields.more() {
		field, wire, _, raw, err := fields.next()
		if err != nil {
			return nil, err
		}
		if field == expected {
			if wire != 2 {
				return nil, errMalformedProtobuf
			}
			return raw, nil
		}
	}
	return nil, nil
}

func decodeTwoU32(input []byte) (uint32, uint32, error) {
	var first, second uint32
	fields := wireFields{input: input}
	for fields.more() {
		field, wire, value, _, err := fields.next()
		if err != nil {
			return 0, 0, err
		}
		switch field {
		case 1:
			if wire != 0 {
				return 0, 0, errMalformedProtobuf
			}
			first = uint32(value)
		case 2:
			if wire != 0 {
				return 0, 0, errMalformedProtobuf
			}
			second = uint32(value)
		}
	}
	return first, second, nil
}

func encodeHello(version string, capabilities []string) []byte {
	var body []byte
	if version != "" {
		body = appendBytesField(body, 1, []byte(version))
	}
	for _, capability := range capabilities {
		if capability != "" {
			body = appendBytesField(body, 2, []byte(capability))
		}
	}
	return body
}

func encodeOutput(channel uint64, data []byte) []byte {
	var body []byte
	if channel != 0 {
		body = appendVarintField(body, 1, channel)
	}
	if len(data) != 0 {
		body = appendBytesField(body, 2, data)
	}
	return body
}

func encodeExit(code int32, signal *int32) []byte {
	var body []byte
	if code != 0 {
		body = appendVarintField(body, 1, uint64(uint32(code)))
	}
	if signal != nil {
		body = appendVarintField(body, 2, uint64(uint32(*signal)))
	}
	return body
}

func encodeShutdown(reason string) []byte {
	if reason == "" {
		return nil
	}
	return appendBytesField(nil, 1, []byte(reason))
}

type wireFields struct {
	input  []byte
	offset int
}

func (f *wireFields) more() bool { return f.offset < len(f.input) }

func (f *wireFields) next() (field int, wire int, value uint64, raw []byte, err error) {
	key, err := f.varint()
	if err != nil || key == 0 {
		return 0, 0, 0, nil, errMalformedProtobuf
	}
	field, wire = int(key>>3), int(key&7)
	switch wire {
	case 0:
		value, err = f.varint()
	case 1:
		if f.offset+8 > len(f.input) {
			err = errMalformedProtobuf
		} else {
			f.offset += 8
		}
	case 2:
		var size uint64
		size, err = f.varint()
		if err == nil && (size > uint64(len(f.input)-f.offset) || size > maxFrameSize) {
			err = errMalformedProtobuf
		}
		if err == nil {
			raw = f.input[f.offset : f.offset+int(size)]
			f.offset += int(size)
		}
	case 5:
		if f.offset+4 > len(f.input) {
			err = errMalformedProtobuf
		} else {
			f.offset += 4
		}
	default:
		err = errMalformedProtobuf
	}
	return field, wire, value, raw, err
}

func (f *wireFields) varint() (uint64, error) {
	var value uint64
	for shift := uint(0); shift < 64; shift += 7 {
		if f.offset >= len(f.input) {
			return 0, errMalformedProtobuf
		}
		current := f.input[f.offset]
		f.offset++
		value |= uint64(current&0x7f) << shift
		if current&0x80 == 0 {
			return value, nil
		}
	}
	return 0, errMalformedProtobuf
}

func appendVarintField(output []byte, field int, value uint64) []byte {
	output = appendVarint(output, uint64(field<<3))
	return appendVarint(output, value)
}

func appendBytesField(output []byte, field int, value []byte) []byte {
	output = appendVarint(output, uint64(field<<3|2))
	output = appendVarint(output, uint64(len(value)))
	return append(output, value...)
}

func appendVarint(output []byte, value uint64) []byte {
	for value >= 0x80 {
		output = append(output, byte(value)|0x80)
		value >>= 7
	}
	return append(output, byte(value))
}

func writeAll(w io.Writer, data []byte) error {
	for len(data) > 0 {
		written, err := w.Write(data)
		if err != nil {
			return err
		}
		if written == 0 {
			return io.ErrShortWrite
		}
		data = data[written:]
	}
	return nil
}
