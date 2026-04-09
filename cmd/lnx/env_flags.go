package main

import (
	"fmt"
	"os"
	"sort"
	"strings"

	"github.com/joho/godotenv"
)

var forwardEnv []string
var forwardAllEnv bool

func execEnv() ([]string, error) {
	if forwardAllEnv {
		var env []string
		for _, kv := range os.Environ() {
			key, _, _ := strings.Cut(kv, "=")
			if excludePreservedEnvKey(key) {
				continue
			}
			env = append(env, kv)
		}
		return env, nil
	}

	env := make([]string, 0, len(forwardEnv))
	for _, spec := range forwardEnv {
		if spec == "" {
			continue
		}
		if strings.HasPrefix(spec, "@") {
			fileEnv, err := loadDotenv(spec[1:])
			if err != nil {
				return nil, err
			}
			env = append(env, fileEnv...)
			continue
		}
		if strings.Contains(spec, "=") {
			env = append(env, spec)
			continue
		}
		value, ok := os.LookupEnv(spec)
		if !ok {
			return nil, fmt.Errorf("host env var %q is not set", spec)
		}
		env = append(env, spec+"="+value)
	}
	return env, nil
}

func excludePreservedEnvKey(key string) bool {
	switch key {
	case "HOME", "PATH", "PWD", "OLDPWD", "TMPDIR", "SHELL",
		"SSH_AUTH_SOCK", "DISPLAY", "XDG_RUNTIME_DIR",
		"SECURITYSESSIONID", "LaunchInstanceID", "COMMAND_MODE":
		return true
	}
	for _, prefix := range []string{"DYLD_", "__CF_", "APPLE_", "XPC_"} {
		if strings.HasPrefix(key, prefix) {
			return true
		}
	}
	return false
}

func loadDotenv(path string) ([]string, error) {
	values, err := godotenv.Read(path)
	if err != nil {
		return nil, fmt.Errorf("read env file %q: %w", path, err)
	}

	keys := make([]string, 0, len(values))
	for key := range values {
		keys = append(keys, key)
	}
	sort.Strings(keys)

	env := make([]string, 0, len(keys))
	for _, key := range keys {
		env = append(env, key+"="+values[key])
	}
	return env, nil
}
