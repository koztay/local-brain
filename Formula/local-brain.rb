# Homebrew formula for local-brain (prebuilt release binaries).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  version "0.1.7"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.7/local-brain-aarch64-apple-darwin.tar.gz"
      sha256 "0a64e3bd1d476e7ba986114a65348da8f38f0201a181eaeb88001906f0e43516"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.7/local-brain-x86_64-apple-darwin.tar.gz"
      sha256 "bff99d617c44518953f387f70c1afa6b463bfa04f9d4e6821e7a1b58e1dbc554"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/koztay/local-brain/releases/download/v0.1.7/local-brain-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "edb4a016de7bb8b4e180af84ae069fe7a107477abefc3d2be97e7c31414938de"
    else
      url "https://github.com/koztay/local-brain/releases/download/v0.1.7/local-brain-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "cdf19f2ecbf46650fd9969dc439f043f3953fd43248b92002163982553a80791"
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
