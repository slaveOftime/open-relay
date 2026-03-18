# typed: false
# frozen_string_literal: true

# Homebrew formula for oly — session-persistent PTY daemon for CLI agents.
# Tap: brew tap slaveOftime/oly
class Oly < Formula
  desc "Session-persistent PTY daemon for long-running CLI agents"
  homepage "https://github.com/slaveOftime/open-relay"
  version "0.1.5"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-macos-arm64.zip"
      # SHA256 is updated automatically by the release workflow.
      sha256 "f93f7c3f2e0bbbd5727d5cf4df8d63e20a3e64f02afcc4e2b3f4126f278ad942"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-linux-amd64.zip"
      sha256 "0ac5b6e38c386cf7979e3aa9436c7829c19cfd774e156eec4f8453603f633f43"
    end
  end

  def install
    bin.install "oly"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oly --version 2>&1", 0)
  end
end
