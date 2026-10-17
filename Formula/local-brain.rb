# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.6"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.6/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "a1a1eabd4f2553b2e03887b93cb4dc8e04afa69480996ec2c43e0eb68c4733c9"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.6/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "fe8141aaf90a5287021a9a02f21e568e4f3ed45bc3a7dec9a8051ec49118ee0f"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.6/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f4d4e28cd320634ae8e34657bf3c8e079acb5d1dd6ec8091d0a534a1fbd5611e"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.6/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "985255ccc7189077a01ebe8e7f933fd7bc91d37dd2664b3f066fc87d6750b54f"
    end
  end

  head do
    url "https://github.com/koztay/local-brain.git", branch: "main"
    depends_on "rust" => :build
  end

  def install
    if build.head?
      system "cargo", "install", *std_cargo_args
    else
      bin.install "local-brain"
    end
  end

  def caveats
    <<~EOS
      In a project: local-brain init && local-brain learn
      init offers to install and configure a local model (Ollama) for summaries,
      local image reading and smarter compression.
    EOS
  end

  test do
    assert_match "local-brain", shell_output("#{bin}/local-brain --version")
  end
end
