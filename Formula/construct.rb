# frozen_string_literal: true

# Homebrew formula for the construct terminal-native agentic development
# environment.
class Construct < Formula
  desc "Terminal-native agentic development environment"
  homepage "https://github.com/construct-worlds/construct"
  version "0.16.8"
  license "MIT"

  depends_on :macos

  if Hardware::CPU.arm?
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-aarch64-apple-darwin.tar.gz"
    sha256 "11825df3f191b9b92d52030b3888af84ecd31c4c483e39672e3251357ae8f174"
  else
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-x86_64-apple-darwin.tar.gz"
    sha256 "fda046ae80c25b7aea5e5de719673c5c6a986ebd5128559efbd415506bfc2732"
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
