# frozen_string_literal: true

# Homebrew formula for the construct terminal-native agentic development
# environment.
class Construct < Formula
  desc "Terminal-native agentic development environment"
  homepage "https://github.com/construct-worlds/construct"
  version "0.17.11"
  license "MIT"

  depends_on :macos

  if Hardware::CPU.arm?
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-aarch64-apple-darwin.tar.gz"
    sha256 "e84dcb2b9e1dd8e044480d291b0398a18beeca67b20204ee0416b1f747676fed"
  else
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-x86_64-apple-darwin.tar.gz"
    sha256 "fbaccd3262aa78e75b6dd9d2f7bcbb8d20c7004652b887a5233791950f6924fe"
  end

  def install
    # Homebrew normally enters the archive's single top-level directory before
    # calling install, but keep the archive-root layout working too.
    binary = ["construct", *Dir["construct-*/construct"]].find do |path|
      File.file?(path)
    end
    raise "construct binary not found in release archive" if binary.nil?

    bin.install binary => "construct"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/construct --version")
  end
end
