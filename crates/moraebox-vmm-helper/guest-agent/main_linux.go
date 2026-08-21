//go:build linux

package main

import (
	"errors"
	"flag"
	"fmt"
	"os"
	"syscall"
	"unsafe"
)

const (
	afVsock       = 40
	vmaddrCIDHost = 2
)

var agentVersion = "development"

type rawSockaddrVM struct {
	family    uint16
	reserved1 uint16
	port      uint32
	cid       uint32
	zero      [4]byte
}

func main() {
	port := flag.Uint("port", 0, "host control vsock port")
	sessionID := flag.String("session", "", "moraebox session ID")
	flag.Parse()
	if *port == 0 || *port > uint(^uint32(0)) || *sessionID == "" {
		fmt.Fprintln(os.Stderr, "morae guest agent: port and session are required")
		os.Exit(125)
	}
	connection, err := connectVsock(uint32(*port))
	if err != nil {
		fmt.Fprintf(os.Stderr, "morae guest agent: connect: %v\n", err)
		os.Exit(125)
	}
	defer connection.Close()
	code, err := serve(connection, *sessionID)
	if err != nil {
		fmt.Fprintf(os.Stderr, "morae guest agent: %v\n", err)
		os.Exit(125)
	}
	os.Exit(code)
}

func serve(connection *os.File, sessionID string) (int, error) {
	writer := &frameWriter{w: connection, sessionID: sessionID}
	if err := writer.send(payloadHello, encodeHello(agentVersion, []string{
		"exec", "stdin", "signal", "resize", "tty", "output-v1",
	})); err != nil {
		return 125, err
	}
	first, err := readFrame(connection)
	if err != nil {
		return 125, err
	}
	if first.sessionID != sessionID || first.streamID != execStreamID || first.sequence != 0 || first.payload != payloadExec {
		return 125, errors.New("invalid initial exec frame")
	}
	request, err := decodeExec(first.body)
	if err != nil {
		return 125, err
	}
	process, err := startProcess(request, writer)
	if err != nil {
		_ = writer.send(payloadOutput, encodeOutput(1, []byte(err.Error()+"\n")))
		_ = writer.send(payloadExit, encodeExit(127, nil))
		return 127, nil
	}

	frames := make(chan wireFrame)
	readErrors := make(chan error, 1)
	go func() {
		for {
			frame, err := readFrame(connection)
			if err != nil {
				readErrors <- err
				return
			}
			frames <- frame
		}
	}()
	waited := make(chan processResult, 1)
	go func() { waited <- process.wait() }()

	nextSequence := uint64(1)
	stdinClosed := false
	for {
		select {
		case frame := <-frames:
			if frame.sessionID != sessionID || frame.streamID != execStreamID || frame.sequence != nextSequence {
				process.kill(syscall.SIGKILL)
				return 125, errors.New("protocol identity or sequence mismatch")
			}
			nextSequence++
			switch frame.payload {
			case payloadStdin:
				if stdinClosed {
					process.kill(syscall.SIGKILL)
					return 125, errors.New("stdin received after EOF")
				}
				data, err := decodeBytesField(frame.body, 1)
				if err != nil || process.writeStdin(data) != nil {
					process.kill(syscall.SIGKILL)
					return 125, errors.New("failed to write workload stdin")
				}
			case payloadStdinEOF:
				if stdinClosed {
					return 125, errors.New("duplicate stdin EOF")
				}
				stdinClosed = true
				_ = process.closeStdin()
			case payloadResize:
				rows, cols, err := decodeTwoU32(frame.body)
				if err != nil || process.resize(rows, cols) != nil {
					return 125, errors.New("failed to resize workload terminal")
				}
			case payloadSignal:
				signal, _, err := decodeTwoU32(frame.body)
				if err != nil || process.signal(signal) != nil {
					return 125, errors.New("failed to signal workload")
				}
			case payloadShutdown:
				process.kill(syscall.SIGTERM)
			default:
				process.kill(syscall.SIGKILL)
				return 125, errors.New("unexpected host payload")
			}
		case result := <-waited:
			if err := writer.send(payloadExit, encodeExit(result.code, result.signal)); err != nil {
				return 125, err
			}
			return result.shellCode(), nil
		case err := <-readErrors:
			process.kill(syscall.SIGKILL)
			return 125, fmt.Errorf("control connection failed: %w", err)
		}
	}
}

func connectVsock(port uint32) (*os.File, error) {
	fd, err := syscall.Socket(afVsock, syscall.SOCK_STREAM|syscall.SOCK_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	sa := rawSockaddrVM{family: afVsock, port: port, cid: vmaddrCIDHost}
	_, _, errno := syscall.Syscall(syscall.SYS_CONNECT, uintptr(fd), uintptr(unsafe.Pointer(&sa)), unsafe.Sizeof(sa))
	if errno != 0 {
		syscall.Close(fd)
		return nil, errno
	}
	return os.NewFile(uintptr(fd), "vsock-control"), nil
}
