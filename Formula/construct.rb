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
    sha256 "934cf8aca749482d265ae946da1945e873d9285af4f48e3244a0f9e85adaba21"
  else
    url "https://github.com/construct-worlds/construct/releases/download/v#{version}/construct-x86_64-apple-darwin.tar.gz"
    sha256 "4f3dadca58b3ffac0fae4b71abd09334cf4b6b65e55ddc2bfb87e6f5ed5d24b8"
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
