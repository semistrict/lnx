# Homebrew formula for the prebuilt lnx binary.
#
# The repo doubles as a tap:
#   brew tap semistrict/lnx https://github.com/semistrict/lnx
#   brew install semistrict/lnx/lnx
#
# When tagging a release, update `version` and `sha256` below to match the
# lnx-macos-arm64.tar.gz asset produced by .github/workflows/release-binary.yml
# (the .sha256 file is published next to it).
class Lnx < Formula
  desc "Linux VMs on macOS that resume with memory and disk state intact"
  homepage "https://github.com/semistrict/lnx"
  version "0.3.0"
  url "https://github.com/semistrict/lnx/releases/download/v#{version}/lnx-macos-arm64.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: set from the release .sha256 asset
  license "Apache-2.0"

  depends_on :macos
  depends_on arch: :arm64

  def install
    bin.install "lnx"
  end

  test do
    assert_match "Linux VM runner", shell_output("#{bin}/lnx --help")
  end
end
