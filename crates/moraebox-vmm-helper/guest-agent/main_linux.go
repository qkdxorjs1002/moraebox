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
		"exec", "stdin", "signal", "resize", "tty", "output-v1", "copy-tar-v1",
	})); err != nil {
		return 125, err
	}
	request, copyOut, nextSequence, err := readInitialRequest(connection, sessionID)
	if err != nil {
		return 125, err
	}
	process, err := startProcess(request, writer)
	if err != nil {
		_ = writer.send(payloadOutput, encodeOutput(1, []byte(err.Error()+"\n")))
		if diffErr := writeWorkspaceDiffIfRequested(copyOut); diffErr != nil {
			_ = writer.send(payloadOutput, encodeOutput(1, []byte(diffErr.Error()+"\n")))
			_ = writer.send(payloadExit, encodeExit(125, nil))
			return 125, nil
		}
		for _, copyRequest := range copyOut {
			if copyErr := sendCopyOut(writer, copyRequest); copyErr != nil {
				_ = writer.send(payloadOutput, encodeOutput(1, []byte(copyErr.Error()+"\n")))
				_ = writer.send(payloadExit, encodeExit(125, nil))
				return 125, nil
			}
		}
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
			if err := writeWorkspaceDiffIfRequested(copyOut); err != nil {
				message := []byte("morae guest agent: workspace diff failed: " + err.Error() + "\n")
				_ = writer.send(payloadOutput, encodeOutput(1, message))
				_ = writer.send(payloadExit, encodeExit(125, nil))
				return 125, nil
			}
			for _, copyRequest := range copyOut {
				if err := sendCopyOut(writer, copyRequest); err != nil {
					message := []byte("morae guest agent: copy-out failed: " + err.Error() + "\n")
					_ = writer.send(payloadOutput, encodeOutput(1, message))
					_ = writer.send(payloadExit, encodeExit(125, nil))
					return 125, nil
				}
			}
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

func readInitialRequest(connection *os.File, sessionID string) (execRequest, []copyOutRequest, uint64, error) {
	var active *inboundCopy
	defer func() {
		if active != nil {
			active.discard()
		}
	}()
	copyOut := make([]copyOutRequest, 0)
	seenTransfers := make(map[uint64]struct{})
	nextSequence := uint64(0)
	for {
		frame, err := readFrame(connection)
		if err != nil {
			return execRequest{}, nil, 0, err
		}
		if frame.sessionID != sessionID || frame.streamID != execStreamID || frame.sequence != nextSequence {
			return execRequest{}, nil, 0, errors.New("invalid initial protocol identity or sequence")
		}
		nextSequence++
		switch frame.payload {
		case payloadCopyInStart:
			if active != nil {
				return execRequest{}, nil, 0, errors.New("copy-in transfer is already active")
			}
			start, err := decodeCopyInStart(frame.body)
			if err != nil {
				return execRequest{}, nil, 0, err
			}
			if _, duplicate := seenTransfers[start.transferID]; duplicate {
				return execRequest{}, nil, 0, errors.New("duplicate transfer id")
			}
			seenTransfers[start.transferID] = struct{}{}
			active, err = newInboundCopy(start)
			if err != nil {
				return execRequest{}, nil, 0, err
			}
		case payloadCopyInChunk:
			if active == nil {
				return execRequest{}, nil, 0, errors.New("copy-in chunk has no active transfer")
			}
			chunk, err := decodeCopyChunk(frame.body)
			if err != nil || active.append(chunk) != nil {
				return execRequest{}, nil, 0, errors.New("invalid copy-in chunk")
			}
		case payloadCopyInEnd:
			if active == nil {
				return execRequest{}, nil, 0, errors.New("copy-in end has no active transfer")
			}
			transferID, err := decodeTransferID(frame.body)
			if err != nil {
				return execRequest{}, nil, 0, err
			}
			if err := active.finish(transferID); err != nil {
				return execRequest{}, nil, 0, err
			}
			active = nil
		case payloadCopyOutRequest:
			if active != nil {
				return execRequest{}, nil, 0, errors.New("copy-out requested during copy-in")
			}
			request, err := decodeCopyOutRequest(frame.body)
			if err != nil {
				return execRequest{}, nil, 0, err
			}
			if _, duplicate := seenTransfers[request.transferID]; duplicate {
				return execRequest{}, nil, 0, errors.New("duplicate transfer id")
			}
			seenTransfers[request.transferID] = struct{}{}
			copyOut = append(copyOut, request)
		case payloadExec:
			if active != nil {
				return execRequest{}, nil, 0, errors.New("exec received during copy-in")
			}
			request, err := decodeExec(frame.body)
			return request, copyOut, nextSequence, err
		default:
			return execRequest{}, nil, 0, errors.New("unexpected initial host payload")
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
