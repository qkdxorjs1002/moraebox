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
