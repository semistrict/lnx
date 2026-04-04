module github.com/semistrict/lnx

go 1.26.1

replace github.com/Code-Hex/vz/v3 => ./third_party/vz

require (
	github.com/Code-Hex/vz/v3 v3.7.1
	github.com/creack/pty v1.1.24
	github.com/mdlayher/vsock v1.2.1
	github.com/spf13/cobra v1.10.2
	github.com/stretchr/testify v1.11.1
	golang.org/x/sys v0.42.0
	golang.org/x/term v0.41.0
)

require (
	github.com/Code-Hex/go-infinity-channel v1.0.0 // indirect
	github.com/davecgh/go-spew v1.1.1 // indirect
	github.com/google/go-cmp v0.7.0 // indirect
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/mdlayher/socket v0.4.1 // indirect
	github.com/pmezard/go-difflib v1.0.0 // indirect
	github.com/spf13/pflag v1.0.10 // indirect
	golang.org/x/mod v0.34.0 // indirect
	golang.org/x/net v0.44.0 // indirect
	golang.org/x/sync v0.17.0 // indirect
	gopkg.in/yaml.v3 v3.0.1 // indirect
)
