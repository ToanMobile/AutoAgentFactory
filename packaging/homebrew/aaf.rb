class Aaf < Formula
  desc "Receipt-verified autonomous agent factory CLI and PreToolUse hook guard"
  homepage "https://github.com/ToanMobile/AutoAgentFactory"
  version "0.1.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_AARCH64_APPLE_DARWIN"
    end

    on_intel do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_X86_64_APPLE_DARWIN"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256_AARCH64_UNKNOWN_LINUX_GNU"
    end

    on_intel do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256_X86_64_UNKNOWN_LINUX_GNU"
    end
  end

  def install
    bin.install "aaf"
    bin.install "aaf-hook"
    doc.install "README.md" if File.exist?("README.md")
    prefix.install "LICENSE" if File.exist?("LICENSE")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/aaf --version")
    pipe_output("#{bin}/aaf-hook", "not-valid-json\n", 2)
  end
end
