# typed: false
# frozen_string_literal: true

# Homebrew formula for oly — session-persistent PTY daemon for CLI agents.
# Tap: brew tap slaveOftime/oly
class Oly < Formula
  desc "Session-persistent PTY daemon for long-running CLI agents"
  homepage "https://github.com/slaveOftime/open-relay"
  version "0.1.9"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-macos-arm64.zip"
      # SHA256 is updated automatically by the release workflow.
      sha256 "10832d32a44bfa086d8ee9c28e904f8f12d9b25223d50da075185c73eb503e81"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-linux-amd64.zip"
      sha256 "7b23deadeecdcc7d9e8a11af2264434f8139bea6381136999a05a47225a8115d"
    end
  end

  def install
    bin.install "oly"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oly --version 2>&1", 0)
  end
end
