package main

import (
	"os"
	"strings"
)

func experimentEnabled(name string) bool {
	for _, exp := range strings.Split(os.Getenv("LNX_EXPERIMENTS"), ",") {
		if strings.TrimSpace(exp) == name {
			return true
		}
	}
	return false
}
