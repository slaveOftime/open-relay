# typed: false
# frozen_string_literal: true

# Homebrew formula for oly — session-persistent PTY daemon for CLI agents.
# Tap: brew tap slaveOftime/oly
class Oly < Formula
  desc "Session-persistent PTY daemon for long-running CLI agents"
  homepage "https://github.com/slaveOftime/open-relay"
  version "0.1.4"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-macos-arm64.zip"
      # SHA256 is updated automatically by the release workflow.
      sha256 "ef9131f20ff9c34b42b63ece3e5da624f48d24ead9773a8a634116555039678e"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-linux-amd64.zip"
      sha256 "1057af12022ba31533affeb00f1a7069f511b54f44bb6881a75d482da156bf1a"
    end
  end

  def install
    bin.install "oly"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oly --version 2>&1", 0)
  end
end
