package main

import (
	"flag"
	"log"
	"os"

	"github.com/hanwen/go-fuse/v2/fs"
	"github.com/hanwen/go-fuse/v2/virtiofs"
)

func main() {
	socket := flag.String("socket", "", "vhost-user Unix socket path")
	rootPath := flag.String("root", "", "host directory to expose")
	flag.Parse()

	if *socket == "" || *rootPath == "" {
		log.Fatal("usage: vhost-user-fs -socket PATH -root DIR")
	}
	if err := os.RemoveAll(*socket); err != nil {
		log.Fatalf("remove stale socket: %v", err)
	}

	root, err := fs.NewLoopbackRoot(*rootPath)
	if err != nil {
		log.Fatalf("create loopback root: %v", err)
	}
	opts := &fs.Options{}
	rawFS := fs.NewNodeFS(root, opts)
	virtiofs.ServeFS(*socket, rawFS, &opts.MountOptions)
}
