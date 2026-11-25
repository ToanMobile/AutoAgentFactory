class Aaf < Formula
  desc "Receipt-verified autonomous agent factory CLI and PreToolUse hook guard"
  homepage "https://github.com/ToanMobile/AutoAgentFactory"
  version "0.3.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "7957b524a7fdf3749e1c93d97090e4f441b7f11a05673fc3febaed8faeadbadf"
    end

    on_intel do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "eabbf552051c7fea452a4e2656b28d09a834d49e957cda221287302f1a013df0"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "36030c6579d35aafaa2860d6412f69cd9006f9e9aa96721cc10573baf140de56"
    end

    on_intel do
      url "https://github.com/ToanMobile/AutoAgentFactory/releases/download/v#{version}/aaf-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d7fb95e10eb1679e8f6f23ad595da904ae2fec587e59c24d05cef9e95002e3dc"
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
