# frozen_string_literal: true

# Homebrew formula for the construct terminal-native agentic development
# environment.
class Construct < Formula
  desc "Terminal-native agentic development environment"
  homepage "https://github.com/construct-worlds/construct"
  version "0.17.9"
  license "MIT"

  depends_on :macos

  if Hardware::CPU.arm?
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-aarch64-apple-darwin.tar.gz"
    sha256 "56b8fd5ca8b1d2761adcc9a2d0e6ad28b4f441e56208e860bb877c2d04362f4b"
  else
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-x86_64-apple-darwin.tar.gz"
    sha256 "4b3cc95ca76192e714a91f53ac9f1a854609cd0ccd51ba42e279547ee90f1205"
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
