# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.9"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.9/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "52c3535d1af8ac438d0b154517b15a20134077f6245be3865bda86128993187a"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.9/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "8bdd254f95d033983cb139e6b3d709e51824e8eb3abfb10c39212df43b207141"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.9/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "986a1e306bde86d269a8538da47cf1568c8e95e86cf3e9b7c703199566d09838"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.9/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "3d98969b7d1b8eeb3a514c22201c5eaee0a67ad3a19c240ea9b139b551e633e5"
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
