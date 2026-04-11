package main

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/spf13/cobra"
)

var mktempCmd = &cobra.Command{
	Use:   "mktemp [template]",
	Short: "Create a temporary file or directory at a path valid on both host and guest",
	Long: `Create a temporary file or directory under /tmp (by default), producing a
path that resolves on both macOS and Linux. This avoids macOS's default
behavior of creating temps under /var/folders/... which doesn't exist
in the guest.

Supports the mktemp flags common to both macOS and Linux.

The template may contain trailing X's which are replaced with random
characters (e.g., myapp.XXXXXX). If no template is given, "lnx.XXXXXX"
is used.`,
	Args: cobra.MaximumNArgs(1),
	RunE: runMktemp,
}

var (
	mktempDir    bool
	mktempTmpdir string
	mktempQuiet  bool
	mktempDryRun bool
	mktempPrefix string
)

func init() {
	mktempCmd.Flags().BoolVarP(&mktempDir, "directory", "d", false, "Create a directory instead of a file")
	mktempCmd.Flags().StringVarP(&mktempTmpdir, "tmpdir", "p", "/tmp", "Use this directory as the parent")
	mktempCmd.Flags().BoolVarP(&mktempQuiet, "quiet", "q", false, "Suppress error messages")
	mktempCmd.Flags().BoolVarP(&mktempDryRun, "dry-run", "u", false, "Print the path without creating it")
	mktempCmd.Flags().StringVarP(&mktempPrefix, "prefix", "t", "", "Use this prefix (equivalent to template PREFIX.XXXXXX)")

	rootCmd.AddCommand(mktempCmd)
}

func runMktemp(cmd *cobra.Command, args []string) error {
	dir := mktempTmpdir
	template := "lnx.XXXXXX"

	if mktempPrefix != "" {
		template = mktempPrefix + ".XXXXXX"
	}
	if len(args) > 0 {
		template = args[0]
		// If the template contains a directory component, split it.
		if d := filepath.Dir(template); d != "." && d != "" {
			dir = d
			template = filepath.Base(template)
		}
	}

	// Convert trailing X's to Go's temp pattern.
	// mktemp: "foo.XXXXXX" → Go: "foo." (random chars appended)
	// mktemp: "fooXXX" → Go: "foo" (random chars appended)
	pattern := trimTrailingXs(template)

	if mktempDryRun {
		b := make([]byte, 3)
		rand.Read(b)
		fmt.Println(filepath.Join(dir, pattern+hex.EncodeToString(b)))
		return nil
	}

	var path string
	var err error
	if mktempDir {
		path, err = os.MkdirTemp(dir, pattern)
	} else {
		var f *os.File
		f, err = os.CreateTemp(dir, pattern)
		if err == nil {
			path = f.Name()
			f.Close()
		}
	}

	if err != nil {
		if mktempQuiet {
			os.Exit(1)
		}
		return err
	}

	fmt.Println(path)
	return nil
}

// trimTrailingXs removes trailing X's from a template, leaving the
// prefix for Go's os.CreateTemp/os.MkdirTemp which appends random chars.
func trimTrailingXs(s string) string {
	return strings.TrimRight(s, "X")
}
