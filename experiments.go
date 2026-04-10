package lnx

import (
	"os"
	"strings"
)

// ExperimentEnabled reports whether LNX_EXPERIMENTS contains name.
func ExperimentEnabled(name string) bool {
	for _, exp := range strings.Split(os.Getenv("LNX_EXPERIMENTS"), ",") {
		if strings.TrimSpace(exp) == name {
			return true
		}
	}
	return false
}
