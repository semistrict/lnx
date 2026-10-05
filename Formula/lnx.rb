# Homebrew formula for the prebuilt lnx binary.
#
# The repo doubles as a tap:
#   brew tap semistrict/lnx https://github.com/semistrict/lnx
#   brew install semistrict/lnx/lnx
#
# CI updates `version` and `sha256` below on every `v*` tag via the "Update
# Homebrew formula" step in .github/workflows/release-binary.yml.
class Lnx < Formula
  desc "Linux VMs on macOS that resume with memory and disk state intact"
  homepage "https://github.com/semistrict/lnx"
  version "0.3.1"
  url "https://github.com/semistrict/lnx/releases/download/v#{version}/lnx-macos-arm64.tar.gz"
  sha256 "77ae19d86a44e94c9cec29c1aea58347f52261416739f48cb8513e2fa2c1def7"
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
