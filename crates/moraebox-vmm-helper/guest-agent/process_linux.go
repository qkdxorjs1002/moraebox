//go:build linux

package main

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"unsafe"
)

const defaultGuestPath = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

const (
	tiocgptn   = 0x80045430
	tiocsptlck = 0x40045431
	tiocswinsz = 0x5414
)

type windowSize struct {
	rows   uint16
	cols   uint16
	xpixel uint16
	ypixel uint16
}

type guestProcess struct {
	cmd    *exec.Cmd
	stdin  io.WriteCloser
	pty    *os.File
	output sync.WaitGroup
	writer *frameWriter
}

type processResult struct {
	code   int32
	signal *int32
}

func (r processResult) shellCode() int {
	if r.signal != nil {
		return 128 + int(*r.signal)
	}
	if r.code < 0 || r.code > 255 {
		return 125
	}
	return int(r.code)
}

func startProcess(request execRequest, writer *frameWriter) (*guestProcess, error) {
	executable, err := resolveExecutable(request.argv[0], request.env)
	if err != nil {
		return nil, err
	}
	cmd := &exec.Cmd{Path: executable, Args: request.argv}
	cmd.Dir = request.cwd
	cmd.Env = make([]string, len(request.env))
	copy(cmd.Env, request.env)
	process := &guestProcess{cmd: cmd, writer: writer}
	if request.tty {
		master, slave, err := openPTY(request.rows, request.cols)
		if err != nil {
			return nil, err
		}
		process.pty = master
		process.stdin = master
		cmd.Stdin = slave
		cmd.Stdout = slave
		cmd.Stderr = slave
		cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Setctty: true, Ctty: 0, Pdeathsig: syscall.SIGKILL}
		if err := cmd.Start(); err != nil {
			master.Close()
			slave.Close()
			return nil, err
		}
		slave.Close()
		process.pipeOutput(master, 2)
		return process, nil
	}

	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, stdoutWriter, err := os.Pipe()
	if err != nil {
		stdin.Close()
		return nil, err
	}
	stderr, stderrWriter, err := os.Pipe()
	if err != nil {
		stdin.Close()
		stdout.Close()
		stdoutWriter.Close()
		return nil, err
	}
	cmd.Stdout = stdoutWriter
	cmd.Stderr = stderrWriter
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true, Pdeathsig: syscall.SIGKILL}
	if err := cmd.Start(); err != nil {
		stdin.Close()
		stdout.Close()
		stdoutWriter.Close()
		stderr.Close()
		stderrWriter.Close()
		return nil, err
	}
	stdoutWriter.Close()
	stderrWriter.Close()
	process.stdin = stdin
	process.pipeOutput(stdout, 0)
	process.pipeOutput(stderr, 1)
	return process, nil
}

func resolveExecutable(name string, environment []string) (string, error) {
	if strings.ContainsRune(name, '/') {
		return name, nil
	}
	searchPath := defaultGuestPath
	for _, value := range environment {
		if strings.HasPrefix(value, "PATH=") {
			searchPath = strings.TrimPrefix(value, "PATH=")
			break
		}
	}
	for _, directory := range filepath.SplitList(searchPath) {
		if directory == "" {
			directory = "."
		}
		candidate := filepath.Join(directory, name)
		info, err := os.Stat(candidate)
		if err == nil && !info.IsDir() && info.Mode().Perm()&0111 != 0 {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("executable %q was not found in guest PATH", name)
}

func (p *guestProcess) pipeOutput(reader io.Reader, channel uint64) {
	p.output.Add(1)
	go func() {
		defer p.output.Done()
		if closer, ok := reader.(io.Closer); ok {
			defer closer.Close()
		}
		buffer := make([]byte, 32*1024)
		for {
			count, err := reader.Read(buffer)
			if count > 0 {
				if sendErr := p.writer.send(payloadOutput, encodeOutput(channel, buffer[:count])); sendErr != nil {
					return
				}
			}
			if err != nil {
				return
			}
		}
	}()
}

func (p *guestProcess) wait() processResult {
	err := p.cmd.Wait()
	p.output.Wait()
	if p.pty != nil {
		p.pty.Close()
	}
	if err == nil {
		return processResult{code: 0}
	}
	var exitError *exec.ExitError
	if !errors.As(err, &exitError) {
		return processResult{code: 125}
	}
	status, ok := exitError.Sys().(syscall.WaitStatus)
	if !ok {
		return processResult{code: int32(exitError.ExitCode())}
	}
	if status.Signaled() {
		signal := int32(status.Signal())
		return processResult{signal: &signal}
	}
	return processResult{code: int32(status.ExitStatus())}
}

func (p *guestProcess) writeStdin(data []byte) error {
	return writeAll(p.stdin, data)
}

func (p *guestProcess) closeStdin() error {
	if p.pty != nil {
		return nil
	}
	return p.stdin.Close()
}

func (p *guestProcess) resize(rows, cols uint32) error {
	if p.pty == nil {
		return errors.New("workload has no terminal")
	}
	if rows == 0 || cols == 0 || rows > 65535 || cols > 65535 {
		return errors.New("invalid terminal size")
	}
	window := windowSize{rows: uint16(rows), cols: uint16(cols)}
	_, _, errno := syscall.Syscall(syscall.SYS_IOCTL, p.pty.Fd(), tiocswinsz, uintptr(unsafe.Pointer(&window)))
	if errno != 0 {
		return errno
	}
	return nil
}

func (p *guestProcess) signal(wire uint32) error {
	var signal syscall.Signal
	switch wire {
	case 0:
		signal = syscall.SIGINT
	case 1:
		signal = syscall.SIGTERM
	case 2:
		signal = syscall.SIGKILL
	case 3:
		signal = syscall.SIGHUP
	default:
		return errors.New("unsupported signal")
	}
	return p.kill(signal)
}

func (p *guestProcess) kill(signal syscall.Signal) error {
	if p.cmd.Process == nil {
		return nil
	}
	err := syscall.Kill(-p.cmd.Process.Pid, signal)
	if errors.Is(err, syscall.ESRCH) {
		return nil
	}
	return err
}

func openPTY(rows, cols uint32) (*os.File, *os.File, error) {
	fd, err := syscall.Open("/dev/ptmx", syscall.O_RDWR|syscall.O_NOCTTY|syscall.O_CLOEXEC, 0)
	if err != nil {
		return nil, nil, err
	}
	master := os.NewFile(uintptr(fd), "ptmx")
	locked := int32(0)
	_, _, errno := syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), tiocsptlck, uintptr(unsafe.Pointer(&locked)))
	if errno != 0 {
		master.Close()
		return nil, nil, errno
	}
	var number uint32
	_, _, errno = syscall.Syscall(syscall.SYS_IOCTL, uintptr(fd), tiocgptn, uintptr(unsafe.Pointer(&number)))
	if errno != 0 {
		master.Close()
		return nil, nil, errno
	}
	slave, err := os.OpenFile("/dev/pts/"+strconv.FormatUint(uint64(number), 10), os.O_RDWR|syscall.O_NOCTTY, 0)
	if err != nil {
		master.Close()
		return nil, nil, err
	}
	if rows != 0 && cols != 0 {
		window := windowSize{rows: uint16(rows), cols: uint16(cols)}
		_, _, errno = syscall.Syscall(syscall.SYS_IOCTL, master.Fd(), tiocswinsz, uintptr(unsafe.Pointer(&window)))
		if errno != 0 {
			master.Close()
			slave.Close()
			return nil, nil, errno
		}
	}
	return master, slave, nil
}
