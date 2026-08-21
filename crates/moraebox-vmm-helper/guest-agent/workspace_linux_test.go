//go:build linux

package main

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestWorkspaceDiffReportsAddModifyDelete(t *testing.T) {
	state := t.TempDir()
	lower := filepath.Join(state, "lower")
	upper := filepath.Join(state, "upper")
	if err := os.MkdirAll(filepath.Join(lower, "directory"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(upper, "directory"), 0o755); err != nil {
		t.Fatal(err)
	}
	for path, value := range map[string]string{
		filepath.Join(lower, "modified"):                 "before",
		filepath.Join(upper, "modified"):                 "after",
		filepath.Join(lower, "directory", "deleted"):     "gone",
		filepath.Join(upper, "directory", ".wh.deleted"): "",
		filepath.Join(upper, "added"):                    "new",
	} {
		if err := os.WriteFile(path, []byte(value), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	manifest, err := buildWorkspaceDiff(lower, upper)
	if err != nil {
		t.Fatal(err)
	}
	got := make(map[string][2]string)
	for _, entry := range manifest.Entries {
		got[entry.Path] = [2]string{entry.Change, entry.Kind}
	}
	want := map[string][2]string{
		"added":             {"added", "file"},
		"directory":         {"modified", "directory"},
		"directory/deleted": {"deleted", "file"},
		"modified":          {"modified", "file"},
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("diff = %#v, want %#v", got, want)
	}
}

func TestWorkspaceMountPointMustBeARealDirectory(t *testing.T) {
	state := t.TempDir()
	mountPoint := filepath.Join(state, "mount-point")
	if err := ensureRealDirectory(mountPoint, 0o700); err != nil {
		t.Fatalf("create mount point: %v", err)
	}
	if err := os.WriteFile(filepath.Join(mountPoint, "payload"), []byte("payload"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := ensureRealDirectory(mountPoint, 0o700); err != nil {
		t.Fatalf("reuse non-empty mount point: %v", err)
	}

	link := filepath.Join(state, "link")
	if err := os.Symlink(mountPoint, link); err != nil {
		t.Fatal(err)
	}
	if err := ensureRealDirectory(link, 0o700); err == nil {
		t.Fatal("symlink mount point was accepted")
	}
}
