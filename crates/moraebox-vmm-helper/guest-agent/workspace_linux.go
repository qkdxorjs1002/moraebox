//go:build linux

package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"
)

const (
	workspaceRuntime  = "/run/moraebox-workspace"
	workspaceUpper    = workspaceRuntime + "/upper"
	workspaceWork     = workspaceRuntime + "/work"
	workspaceLower    = "/workspace.lower"
	workspaceDiffPath = workspaceRuntime + "/diff.json"
)

func setupWorkspace(device string, writable bool) error {
	if device == "" {
		return nil
	}
	if writable {
		return setupWritableWorkspace(device)
	}
	if err := ensureRealDirectory("/workspace", 0o755); err != nil {
		return err
	}
	if err := syscall.Mount(device, "/workspace", "ext4", syscall.MS_RDONLY|syscall.MS_NOSUID|syscall.MS_NODEV, ""); err != nil {
		return fmt.Errorf("mount immutable workspace: %w", err)
	}
	return nil
}

func setupWritableWorkspace(device string) error {
	for _, directory := range []string{workspaceLower, "/workspace"} {
		if err := ensureRealDirectory(directory, 0o755); err != nil {
			return err
		}
	}
	if err := ensureRealDirectory("/run", 0o755); err != nil {
		return err
	}
	if err := ensureRealDirectory(workspaceRuntime, 0o700); err != nil {
		return err
	}
	if err := syscall.Mount(device, workspaceLower, "ext4", syscall.MS_RDONLY|syscall.MS_NOSUID|syscall.MS_NODEV, ""); err != nil {
		return fmt.Errorf("mount immutable workspace lower: %w", err)
	}
	if err := syscall.Mount("tmpfs", workspaceRuntime, "tmpfs", syscall.MS_NOSUID|syscall.MS_NODEV, "mode=0700"); err != nil {
		return fmt.Errorf("mount workspace tmpfs: %w", err)
	}
	for _, directory := range []string{workspaceUpper, workspaceWork} {
		if err := ensureEmptyMountDirectory(directory, 0o700); err != nil {
			return err
		}
	}
	options := "lowerdir=" + workspaceLower + ",upperdir=" + workspaceUpper + ",workdir=" + workspaceWork
	if err := syscall.Mount("overlay", "/workspace", "overlay", syscall.MS_NOSUID|syscall.MS_NODEV, options); err != nil {
		return fmt.Errorf("mount writable workspace overlay: %w", err)
	}
	return nil
}

func ensureRealDirectory(path string, mode os.FileMode) error {
	info, err := os.Lstat(path)
	if errors.Is(err, os.ErrNotExist) {
		if err := os.Mkdir(path, mode); err != nil {
			return fmt.Errorf("create workspace directory %q: %w", path, err)
		}
		return nil
	}
	if err != nil {
		return fmt.Errorf("inspect workspace directory %q: %w", path, err)
	}
	if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("workspace path %q is not a real directory", path)
	}
	return nil
}

func ensureEmptyMountDirectory(path string, mode os.FileMode) error {
	if err := ensureRealDirectory(path, mode); err != nil {
		return err
	}
	entries, err := os.ReadDir(path)
	if err != nil {
		return fmt.Errorf("inspect workspace mount point %q: %w", path, err)
	}
	if len(entries) != 0 {
		return fmt.Errorf("workspace mount point %q is not empty", path)
	}
	return nil
}

type workspaceDiff struct {
	Version uint64               `json:"version"`
	Entries []workspaceDiffEntry `json:"entries"`
}

type workspaceDiffEntry struct {
	Path   string `json:"path"`
	Change string `json:"change"`
	Kind   string `json:"kind"`
}

func writeWorkspaceDiffIfRequested(requests []copyOutRequest) error {
	requested := false
	for _, request := range requests {
		if request.source == workspaceDiffPath {
			requested = true
			break
		}
	}
	if !requested {
		return nil
	}
	if info, err := os.Stat(workspaceUpper); err != nil || !info.IsDir() {
		return errors.New("workspace diff requested without an active writable overlay")
	}
	manifest, err := buildWorkspaceDiff(workspaceLower, workspaceUpper)
	if err != nil {
		return err
	}
	encoded, err := json.Marshal(manifest)
	if err != nil {
		return fmt.Errorf("encode workspace diff: %w", err)
	}
	temporary, err := os.CreateTemp(workspaceRuntime, ".diff-*.json")
	if err != nil {
		return fmt.Errorf("create workspace diff: %w", err)
	}
	temporaryPath := temporary.Name()
	failed := true
	defer func() {
		_ = temporary.Close()
		if failed {
			_ = os.Remove(temporaryPath)
		}
	}()
	if _, err := temporary.Write(encoded); err != nil {
		return fmt.Errorf("write workspace diff: %w", err)
	}
	if err := temporary.Sync(); err != nil {
		return fmt.Errorf("sync workspace diff: %w", err)
	}
	if err := temporary.Close(); err != nil {
		return fmt.Errorf("close workspace diff: %w", err)
	}
	if err := os.Rename(temporaryPath, workspaceDiffPath); err != nil {
		return fmt.Errorf("publish workspace diff: %w", err)
	}
	failed = false
	return nil
}

func buildWorkspaceDiff(lower, upper string) (workspaceDiff, error) {
	entries := make([]workspaceDiffEntry, 0)
	err := filepath.WalkDir(upper, func(current string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		if current == upper {
			return nil
		}
		if len(entries) >= maxCopyEntries {
			return errors.New("workspace diff contains too many entries")
		}
		relative, err := filepath.Rel(upper, current)
		if err != nil || relative == "." || strings.HasPrefix(relative, "..") {
			return errors.New("workspace diff path escapes the overlay upper")
		}
		info, err := entry.Info()
		if err != nil {
			return err
		}
		whiteout := isOverlayWhiteout(info)
		base := filepath.Base(relative)
		if strings.HasPrefix(base, ".wh.") {
			whiteout = true
			relative = filepath.Join(filepath.Dir(relative), strings.TrimPrefix(base, ".wh."))
		}
		guestPath := filepath.ToSlash(relative)
		lowerPath := filepath.Join(lower, relative)
		if whiteout {
			entries = append(entries, workspaceDiffEntry{
				Path: guestPath, Change: "deleted", Kind: lowerEntryKind(lowerPath),
			})
			return nil
		}
		kind, err := workspaceEntryKind(info)
		if err != nil {
			return fmt.Errorf("workspace diff %q: %w", guestPath, err)
		}
		change := "modified"
		if _, err := os.Lstat(lowerPath); errors.Is(err, os.ErrNotExist) {
			change = "added"
		} else if err != nil {
			return err
		}
		entries = append(entries, workspaceDiffEntry{Path: guestPath, Change: change, Kind: kind})
		return nil
	})
	if err != nil {
		return workspaceDiff{}, fmt.Errorf("scan workspace overlay: %w", err)
	}
	return workspaceDiff{Version: 1, Entries: entries}, nil
}

func isOverlayWhiteout(info os.FileInfo) bool {
	if info.Mode()&os.ModeCharDevice == 0 {
		return false
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	return ok && stat.Rdev == 0
}

func workspaceEntryKind(info os.FileInfo) (string, error) {
	switch {
	case info.Mode().IsRegular():
		return "file", nil
	case info.IsDir():
		return "directory", nil
	case info.Mode()&os.ModeSymlink != 0:
		return "symlink", nil
	default:
		return "", errors.New("unsupported overlay entry type")
	}
}

func lowerEntryKind(path string) string {
	info, err := os.Lstat(path)
	if err != nil {
		return "unknown"
	}
	kind, err := workspaceEntryKind(info)
	if err != nil {
		return "unknown"
	}
	return kind
}
