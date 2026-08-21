//go:build linux

package main

import (
	"archive/tar"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"hash"
	"io"
	"os"
	"path"
	"path/filepath"
	"strings"
)

const (
	copyChunkSize  = 64 * 1024
	maxCopyEntries = 100_000
)

type inboundCopy struct {
	request copyInStart
	file    *os.File
	hash    hash.Hash
	written uint64
}

func newInboundCopy(request copyInStart) (*inboundCopy, error) {
	file, err := os.CreateTemp("", "morae-copy-in-*.tar")
	if err != nil {
		return nil, fmt.Errorf("create copy-in staging archive: %w", err)
	}
	return &inboundCopy{request: request, file: file, hash: sha256.New()}, nil
}

func (copy *inboundCopy) append(chunk copyChunk) error {
	if chunk.transferID != copy.request.transferID {
		return errors.New("copy-in transfer id mismatch")
	}
	next := copy.written + uint64(len(chunk.data))
	if next < copy.written || next > copy.request.archiveSize {
		return errors.New("copy-in archive exceeds its declared size")
	}
	if _, err := copy.file.Write(chunk.data); err != nil {
		return fmt.Errorf("write copy-in staging archive: %w", err)
	}
	if _, err := copy.hash.Write(chunk.data); err != nil {
		return fmt.Errorf("hash copy-in staging archive: %w", err)
	}
	copy.written = next
	return nil
}

func (copy *inboundCopy) finish(transferID uint64) error {
	defer copy.discard()
	if transferID != copy.request.transferID {
		return errors.New("copy-in transfer id mismatch")
	}
	if copy.written != copy.request.archiveSize {
		return fmt.Errorf("copy-in archive size is %d, expected %d", copy.written, copy.request.archiveSize)
	}
	if "sha256:"+hex.EncodeToString(copy.hash.Sum(nil)) != strings.ToLower(copy.request.digest) {
		return errors.New("copy-in archive digest mismatch")
	}
	if _, err := copy.file.Seek(0, io.SeekStart); err != nil {
		return fmt.Errorf("rewind copy-in archive: %w", err)
	}
	if err := extractCopyArchive(copy.file, copy.request.destination); err != nil {
		return fmt.Errorf("extract copy-in archive: %w", err)
	}
	return nil
}

func (copy *inboundCopy) discard() {
	if copy == nil || copy.file == nil {
		return
	}
	name := copy.file.Name()
	_ = copy.file.Close()
	_ = os.Remove(name)
}

type boundedWriter struct {
	w       io.Writer
	written uint64
	limit   uint64
}

func (writer *boundedWriter) Write(data []byte) (int, error) {
	remaining := writer.limit - writer.written
	if uint64(len(data)) > remaining {
		return 0, errors.New("copy archive exceeds the requested byte limit")
	}
	count, err := writer.w.Write(data)
	writer.written += uint64(count)
	return count, err
}

func sendCopyOut(writer *frameWriter, request copyOutRequest) error {
	archive, size, digest, err := createCopyArchive(request.source, request.maxBytes)
	if err != nil {
		return err
	}
	defer func() {
		name := archive.Name()
		_ = archive.Close()
		_ = os.Remove(name)
	}()
	if err := writer.send(payloadCopyOutStart, encodeTransferID(request.transferID)); err != nil {
		return err
	}
	buffer := make([]byte, copyChunkSize)
	for {
		count, readErr := archive.Read(buffer)
		if count > 0 {
			if err := writer.send(payloadCopyOutChunk, encodeCopyChunk(request.transferID, buffer[:count])); err != nil {
				return err
			}
		}
		if errors.Is(readErr, io.EOF) {
			break
		}
		if readErr != nil {
			return fmt.Errorf("read copy-out archive: %w", readErr)
		}
	}
	return writer.send(payloadCopyOutEnd, encodeCopyOutEnd(request.transferID, size, digest))
}

func createCopyArchive(source string, limit uint64) (*os.File, uint64, string, error) {
	if !validGuestPath(source) || limit == 0 || limit > maxTransferSize {
		return nil, 0, "", errors.New("invalid copy-out request")
	}
	info, err := os.Lstat(source)
	if err != nil {
		return nil, 0, "", fmt.Errorf("inspect copy-out source: %w", err)
	}
	archive, err := os.CreateTemp("", "morae-copy-out-*.tar")
	if err != nil {
		return nil, 0, "", fmt.Errorf("create copy-out staging archive: %w", err)
	}
	failed := true
	defer func() {
		if failed {
			name := archive.Name()
			_ = archive.Close()
			_ = os.Remove(name)
		}
	}()
	hasher := sha256.New()
	bounded := &boundedWriter{w: io.MultiWriter(archive, hasher), limit: limit}
	tarWriter := tar.NewWriter(bounded)
	entries := 0
	err = filepath.WalkDir(source, func(current string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		entries++
		if entries > maxCopyEntries {
			return errors.New("copy-out source contains too many entries")
		}
		currentInfo, err := entry.Info()
		if err != nil {
			return err
		}
		relative, err := filepath.Rel(source, current)
		if err != nil {
			return err
		}
		name := "root"
		if relative != "." {
			name += "/" + filepath.ToSlash(relative)
		}
		link := ""
		if currentInfo.Mode()&os.ModeSymlink != 0 {
			link, err = os.Readlink(current)
			if err != nil {
				return err
			}
			if !validArchiveSymlink(name, link) {
				return fmt.Errorf("copy-out symlink %q escapes the source", current)
			}
		}
		if !currentInfo.Mode().IsRegular() && !currentInfo.IsDir() && currentInfo.Mode()&os.ModeSymlink == 0 {
			return fmt.Errorf("copy-out source contains unsupported file type at %q", current)
		}
		header, err := tar.FileInfoHeader(currentInfo, link)
		if err != nil {
			return err
		}
		header.Name = name
		header.Uid = 0
		header.Gid = 0
		header.Uname = ""
		header.Gname = ""
		if err := tarWriter.WriteHeader(header); err != nil {
			return err
		}
		if !currentInfo.Mode().IsRegular() {
			return nil
		}
		file, err := os.Open(current)
		if err != nil {
			return err
		}
		_, copyErr := io.Copy(tarWriter, file)
		closeErr := file.Close()
		if copyErr != nil {
			return copyErr
		}
		return closeErr
	})
	if err == nil {
		err = tarWriter.Close()
	} else {
		_ = tarWriter.Close()
	}
	if err != nil {
		return nil, 0, "", fmt.Errorf("build copy-out archive: %w", err)
	}
	if info.IsDir() && entries == 0 {
		return nil, 0, "", errors.New("copy-out directory traversal produced no root")
	}
	if _, err := archive.Seek(0, io.SeekStart); err != nil {
		return nil, 0, "", fmt.Errorf("rewind copy-out archive: %w", err)
	}
	failed = false
	return archive, bounded.written, "sha256:" + hex.EncodeToString(hasher.Sum(nil)), nil
}

func extractCopyArchive(reader io.Reader, destination string) error {
	if !validGuestPath(destination) {
		return errors.New("invalid copy-in destination")
	}
	if _, err := os.Lstat(destination); err == nil {
		return errors.New("copy-in destination already exists")
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}
	parent := filepath.Dir(destination)
	if err := os.MkdirAll(parent, 0o755); err != nil {
		return fmt.Errorf("create copy-in parent: %w", err)
	}
	staging, err := os.MkdirTemp(parent, ".morae-copy-*")
	if err != nil {
		return fmt.Errorf("create copy-in staging directory: %w", err)
	}
	defer os.RemoveAll(staging)
	seen := make(map[string]struct{})
	directories := make([]struct {
		path string
		mode os.FileMode
	}, 0)
	tarReader := tar.NewReader(reader)
	entries := 0
	for {
		header, err := tarReader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		entries++
		if entries > maxCopyEntries {
			return errors.New("copy-in archive contains too many entries")
		}
		name := path.Clean(header.Name)
		if name != header.Name || (name != "root" && !strings.HasPrefix(name, "root/")) {
			return fmt.Errorf("unsafe copy-in archive path %q", header.Name)
		}
		if _, duplicate := seen[name]; duplicate {
			return fmt.Errorf("duplicate copy-in archive path %q", name)
		}
		seen[name] = struct{}{}
		target := filepath.Join(staging, filepath.FromSlash(name))
		if err := ensureDirectoryPath(staging, filepath.Dir(target)); err != nil {
			return err
		}
		mode := os.FileMode(header.Mode) & 0o777
		switch header.Typeflag {
		case tar.TypeDir:
			if err := os.Mkdir(target, 0o700); err != nil {
				return err
			}
			directories = append(directories, struct {
				path string
				mode os.FileMode
			}{target, mode})
		case tar.TypeReg, tar.TypeRegA:
			file, err := os.OpenFile(target, os.O_WRONLY|os.O_CREATE|os.O_EXCL, mode)
			if err != nil {
				return err
			}
			_, copyErr := io.Copy(file, tarReader)
			closeErr := file.Close()
			if copyErr != nil {
				return copyErr
			}
			if closeErr != nil {
				return closeErr
			}
		case tar.TypeSymlink:
			if !validArchiveSymlink(name, header.Linkname) {
				return fmt.Errorf("unsafe copy-in symlink %q", name)
			}
			if err := os.Symlink(header.Linkname, target); err != nil {
				return err
			}
		default:
			return fmt.Errorf("unsupported copy-in archive entry type %d", header.Typeflag)
		}
	}
	root := filepath.Join(staging, "root")
	if _, err := os.Lstat(root); err != nil {
		return errors.New("copy-in archive has no root entry")
	}
	for index := len(directories) - 1; index >= 0; index-- {
		if err := os.Chmod(directories[index].path, directories[index].mode); err != nil {
			return err
		}
	}
	if err := os.Rename(root, destination); err != nil {
		return fmt.Errorf("install copy-in destination: %w", err)
	}
	return nil
}

func ensureDirectoryPath(root, directory string) error {
	relative, err := filepath.Rel(root, directory)
	if err != nil || relative == ".." || strings.HasPrefix(relative, ".."+string(filepath.Separator)) {
		return errors.New("copy archive path escapes staging")
	}
	current := root
	if relative == "." {
		return nil
	}
	for _, component := range strings.Split(relative, string(filepath.Separator)) {
		current = filepath.Join(current, component)
		info, err := os.Lstat(current)
		if err != nil {
			return fmt.Errorf("copy archive parent is missing: %w", err)
		}
		if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
			return errors.New("copy archive parent is not a real directory")
		}
	}
	return nil
}

func validArchiveSymlink(name, target string) bool {
	if target == "" || path.IsAbs(target) || strings.ContainsRune(target, '\x00') {
		return false
	}
	resolved := path.Clean(path.Join(path.Dir(name), target))
	return resolved == "root" || strings.HasPrefix(resolved, "root/")
}
