# Homebrew formula (HEAD build from source).
#   brew tap koztay/local-brain https://github.com/koztay/local-brain
#   brew install --HEAD local-brain
class LocalBrain < Formula
  desc "Local helper brain for coding agents: code graph, search, summaries and compression"
  homepage "https://github.com/koztay/local-brain"
  license "Apache-2.0"
  head "https://github.com/koztay/local-brain.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      Optional: install a local model runtime such as Ollama (brew install ollama) for
      summaries, local image reading and smarter compression.
      Then in a project: local-brain init && local-brain learn
    EOS
  end

  test do
    assert_match version.to_s.split("-").first, shell_output("#{bin}/local-brain --version")
  end
end
