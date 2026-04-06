package main

import godaemon "github.com/sevlyar/go-daemon"

func daemonizeCurrentProcess() (bool, *godaemon.Context, error) {
	ctx := &godaemon.Context{Umask: 027}
	child, err := ctx.Reborn()
	if err != nil {
		return false, nil, err
	}
	if child != nil {
		return true, ctx, nil
	}
	return false, ctx, nil
}
