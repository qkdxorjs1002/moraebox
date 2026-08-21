//go:build linux

package main

import (
	"archive/tar"
	"bytes"
	"os"
	"path/filepath"
	"testing"
)

func TestCopyArchiveRoundTrip(t *testing.T) {
	state := t.TempDir()
	source := filepath.Join(state, "source")
	if err := os.MkdirAll(filepath.Join(source, "nested"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "nested", "value.txt"), []byte("hello"), 0o640); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("nested/value.txt", filepath.Join(source, "link")); err != nil {
		t.Fatal(err)
	}
	archive, size, digest, err := createCopyArchive(source, 1024*1024)
	if err != nil {
		t.Fatal(err)
	}
	defer os.Remove(archive.Name())
	defer archive.Close()
	if size == 0 || !validDigest(digest) {
		t.Fatalf("invalid archive metadata: size=%d digest=%q", size, digest)
	}
	destination := filepath.Join(state, "destination")
	if err := extractCopyArchive(archive, destination); err != nil {
		t.Fatal(err)
	}
	value, err := os.ReadFile(filepath.Join(destination, "nested", "value.txt"))
	if err != nil || string(value) != "hello" {
		t.Fatalf("round-trip value = %q, err = %v", value, err)
	}
	target, err := os.Readlink(filepath.Join(destination, "link"))
	if err != nil || target != "nested/value.txt" {
		t.Fatalf("round-trip link = %q, err = %v", target, err)
	}
}

func TestCopyArchiveRejectsTraversal(t *testing.T) {
	var encoded bytes.Buffer
	writer := tar.NewWriter(&encoded)
	if err := writer.WriteHeader(&tar.Header{Name: "root", Typeflag: tar.TypeDir, Mode: 0o755}); err != nil {
		t.Fatal(err)
	}
	if err := writer.WriteHeader(&tar.Header{Name: "root/../../escape", Typeflag: tar.TypeReg, Mode: 0o600}); err != nil {
		t.Fatal(err)
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	state := t.TempDir()
	if err := extractCopyArchive(bytes.NewReader(encoded.Bytes()), filepath.Join(state, "destination")); err == nil {
		t.Fatal("traversal archive was accepted")
	}
	if _, err := os.Lstat(filepath.Join(state, "escape")); !os.IsNotExist(err) {
		t.Fatalf("archive escaped staging: %v", err)
	}
}

func TestCopyArchiveEnforcesEncodedLimit(t *testing.T) {
	source := filepath.Join(t.TempDir(), "value")
	if err := os.WriteFile(source, bytes.Repeat([]byte("x"), 1024), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, _, _, err := createCopyArchive(source, 512); err == nil {
		t.Fatal("oversized archive was accepted")
	}
}
