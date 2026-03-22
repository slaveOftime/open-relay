# typed: false
# frozen_string_literal: true

# Homebrew formula for oly — session-persistent PTY daemon for CLI agents.
# Tap: brew tap slaveOftime/oly
class Oly < Formula
  desc "Session-persistent PTY daemon for long-running CLI agents"
  homepage "https://github.com/slaveOftime/open-relay"
  version "0.1.8"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-macos-arm64.zip"
      # SHA256 is updated automatically by the release workflow.
      sha256 "64be2fe380baaf0743158624dd9d43ee8c8173da7397de2eb3b095c045dbbce5"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-linux-amd64.zip"
      sha256 "a823eef5f75cd8dc4cfcbb931f9b6042eff15bae163184232f8511d79903ff5b"
    end
  end

  def install
    bin.install "oly"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oly --version 2>&1", 0)
  end
end
