package lnx

import (
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
)

// Opt is a typed runtime option. Declare as a package-level var.
// Read with .Get(). Set via LNX_OPTIONS=key=value or -O key=value.
type Opt[T any] struct {
	Key     string
	Default T
	Desc    string
	parse   func(string) (T, bool)
}

func StringOpt(key, def, desc string) *Opt[string] {
	return &Opt[string]{Key: key, Default: def, Desc: desc, parse: func(s string) (string, bool) { return s, true }}
}

func BoolOpt(key string, def bool, desc string) *Opt[bool] {
	return &Opt[bool]{Key: key, Default: def, Desc: desc, parse: func(s string) (bool, bool) {
		switch strings.ToLower(s) {
		case "1", "true", "yes":
			return true, true
		case "0", "false", "no":
			return false, true
		}
		return false, false
	}}
}

func DurationOpt(key string, def time.Duration, desc string) *Opt[time.Duration] {
	return &Opt[time.Duration]{Key: key, Default: def, Desc: desc, parse: func(s string) (time.Duration, bool) {
		if d, err := time.ParseDuration(s); err == nil {
			return d, true
		}
		if ms, err := strconv.Atoi(s); err == nil && ms > 0 {
			return time.Duration(ms) * time.Millisecond, true
		}
		return 0, false
	}}
}

// Get returns the option's current value (from LNX_OPTIONS, -O flags, or default).
func (o *Opt[T]) Get() T {
	if v, ok := optValues()[o.Key]; ok && v != "" {
		if parsed, ok := o.parse(v); ok {
			return parsed
		}
	}
	return o.Default
}

// --- All options declared here ---

var (
	OptCachePoll            = DurationOpt("cache.poll", 10*time.Second, "host-side invalidation poll interval")
	OptCacheFSEvents        = BoolOpt("cache.fsevents", false, "use macOS FSEvents for invalidation instead of polling")
	OptCacheFSEventsLatency = DurationOpt("cache.fsevents.latency", 500*time.Millisecond, "FSEvents coalescing latency")
)

// --- Option store ---

var store map[string]string

func optValues() map[string]string {
	if store != nil {
		return store
	}
	store = make(map[string]string)
	for _, kv := range strings.Split(os.Getenv("LNX_OPTIONS"), ",") {
		kv = strings.TrimSpace(kv)
		if k, v, ok := strings.Cut(kv, "="); ok {
			store[strings.TrimSpace(k)] = strings.TrimSpace(v)
		}
	}
	return store
}

// SetOption sets a runtime option (e.g., from -O flags). Call before first Get.
func SetOption(key, value string) {
	opts := optValues()
	opts[key] = value
}

// FormatOptionsHelp returns help text listing all declared options.
func FormatOptionsHelp() string {
	all := []interface{ spec() (string, string, string) }{
		wrap(OptCachePoll),
		wrap(OptCacheFSEvents),
		wrap(OptCacheFSEventsLatency),
	}
	var b strings.Builder
	for _, o := range all {
		key, def, desc := o.spec()
		fmt.Fprintf(&b, "  %-30s %s (default: %s)\n", key, desc, def)
	}
	return b.String()
}

type optSpec struct {
	key, def, desc string
}

func (o optSpec) spec() (string, string, string) { return o.key, o.def, o.desc }

func wrap[T any](o *Opt[T]) optSpec {
	return optSpec{key: o.Key, def: fmt.Sprint(o.Default), desc: o.Desc}
}
