# typed: false
# frozen_string_literal: true

# Homebrew formula for oly — session-persistent PTY daemon for CLI agents.
# Tap: brew tap slaveOftime/oly
class Oly < Formula
  desc "Session-persistent PTY daemon for long-running CLI agents"
  homepage "https://github.com/slaveOftime/open-relay"
  version "0.2.3"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-macos-arm64.zip"
      # SHA256 is updated automatically by the release workflow.
      sha256 "d3ecb0be5fd761ec62a66dc96062ae7688b02e05358bbea9210c089167b229e4"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-linux-amd64.zip"
      sha256 "96199e42b00eb687ef319b82623f32314a62d6c806646869cd8d2fe30fbb33be"
    end
  end

  def install
    bin.install "oly"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oly --version 2>&1", 0)
  end
end
