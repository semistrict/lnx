//go:build darwin

package main

import "testing"

func TestParsePlistTopLevel(t *testing.T) {
	plist := `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>VolumeName</key>
	<string>lnx</string>
	<key>CapacityInUse</key>
	<integer>4294967296</integer>
	<key>APFSContainerSize</key>
	<integer>1995165736960</integer>
	<key>APFSContainerFree</key>
	<integer>1063395438592</integer>
	<key>APFSContainerReference</key>
	<string>disk3</string>
	<key>FilesystemType</key>
	<string>apfs</string>
	<key>MountPoint</key>
	<string>/Volumes/lnx</string>
	<key>DeviceIdentifier</key>
	<string>disk3s7</string>
	<key>Encryption</key>
	<true/>
	<key>Removable</key>
	<false/>
	<key>APFSPhysicalStores</key>
	<array>
		<dict>
			<key>APFSPhysicalStore</key>
			<string>disk0s2</string>
		</dict>
	</array>
</dict>
</plist>`

	kv := parsePlistTopLevel([]byte(plist))

	tests := []struct {
		key  string
		want string
	}{
		{"VolumeName", "lnx"},
		{"CapacityInUse", "4294967296"},
		{"APFSContainerSize", "1995165736960"},
		{"APFSContainerFree", "1063395438592"},
		{"APFSContainerReference", "disk3"},
		{"FilesystemType", "apfs"},
		{"MountPoint", "/Volumes/lnx"},
		{"DeviceIdentifier", "disk3s7"},
		{"Encryption", "true"},
		{"Removable", "false"},
	}
	for _, tt := range tests {
		got := kv[tt.key]
		if got != tt.want {
			t.Errorf("key %q = %q, want %q", tt.key, got, tt.want)
		}
	}

	// Nested dict keys should NOT appear at top level.
	if _, ok := kv["APFSPhysicalStore"]; ok {
		t.Error("nested key APFSPhysicalStore should not appear at top level")
	}
}
