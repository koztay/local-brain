# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.1"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.1/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "8d3e42ff148579867ba9e09125af08a86c776f9fe97a8681c38622e101d26922"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.1/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "829210073705b6aff3df495bcb72dabe449c244da27b6bdd6f9ec8a3e210b389"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.1/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "90dda98c6b40cc56bc434657b80bc313885b1dc855b45ea5aa80c3ed29d1eed7"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.1/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "1c2a825f1d7235915d01bc3123a2337f4564a2e95fa248179f1e0e6574ba2e2c"
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
