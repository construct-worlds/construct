# frozen_string_literal: true

# Homebrew formula for the construct terminal-native agentic development
# environment.
class Construct < Formula
  desc "Terminal-native agentic development environment"
  homepage "https://github.com/construct-worlds/construct"
  version "0.17.7"
  license "MIT"

  depends_on :macos

  if Hardware::CPU.arm?
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-aarch64-apple-darwin.tar.gz"
    sha256 "2db7e04ee9ed8df6bea2d800d866fe91fa624b5cc16e89fa2d90990ae491f451"
  else
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-x86_64-apple-darwin.tar.gz"
    sha256 "afb07e4c0d9474112f0aec1614d7513f3de015479dcea160a336f67b0e9d705f"
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
