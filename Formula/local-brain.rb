# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.2"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.2/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "86d6dfa546ad817e14618aa368e8208afcd4f9be73f73e8c31ff09df31c862f5"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.2/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "a0b081a56a0e692281c4121b1073b69cc8cad39c4ddb3168c87bc24c2cb080f4"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.2/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "9e9374727cb44f2d00b97a60def9da402f04456c487b8278c786125159dddffb"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.2/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ea78f811d1e2bbe263fd40811e093f1bac739a6ee93512568dc2adcdde82c2d1"
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
