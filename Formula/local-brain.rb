# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.4"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.4/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "4efe60a324040b2efb7bf1b8c4e6aea013ad5abe3e7d70a39fa2be153346ad25"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.4/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "2ef76127d52f0316ff11a3c5efbbc55fd12dbcd3564cdbf92a2c2fc952f5f306"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.4/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "6a5cd43dfc48cfb62b0bb1e717f22313e7ecca6dea7a5bc09ccca3547407cbca"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.4/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "9063b26d0ce222cd5b9a40555dc91a490d2bcfeb5364a1ca8d99996ed33692d0"
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
