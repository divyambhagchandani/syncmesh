# Seed content for the Homebrew tap formula.
#
# Copy this file to `Formula/syncmesh.rb` in your `homebrew-syncmesh` tap
# repo. After the first real release, the `homebrew-bump` CI job in
# `.github/workflows/release.yml` will keep the `version`, `url`, and
# `sha256` fields up to date automatically.
#
# Users install with:
#   brew tap <your-github-user>/syncmesh
#   brew install syncmesh
#
# `brew install` strips the macOS quarantine attribute on downloaded
# archives, which is why this route sidesteps the Gatekeeper warning
# without needing an Apple Developer ID cert.

class Syncmesh < Formula
  desc "P2P Syncplay alternative for mpv — share playback over an iroh mesh"
  homepage "https://github.com/divyambhagchandani/syncmesh"
  license "MIT OR Apache-2.0"
  version "0.1.0"

  on_macos do
    on_arm do
      url "https://github.com/divyambhagchandani/syncmesh/releases/download/v#{version}/syncmesh-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_ON_FIRST_RELEASE"
    end
    on_intel do
      url "https://github.com/divyambhagchandani/syncmesh/releases/download/v#{version}/syncmesh-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_ON_FIRST_RELEASE"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/divyambhagchandani/syncmesh/releases/download/v#{version}/syncmesh-v#{version}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256_ON_FIRST_RELEASE"
    end
    on_intel do
      url "https://github.com/divyambhagchandani/syncmesh/releases/download/v#{version}/syncmesh-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256_ON_FIRST_RELEASE"
    end
  end

  depends_on "mpv"

  def install
    bin.install "syncmesh"
    (share/"syncmesh").install "scripts/syncmesh.lua" if File.exist?("scripts/syncmesh.lua")
  end

  test do
    assert_match "syncmesh", shell_output("#{bin}/syncmesh --version")
  end
end
