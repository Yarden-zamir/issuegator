# typed: false
# frozen_string_literal: true

class Issuegator < Formula
  desc "Rust TUI GitHub issue explorer for the current repository"
  homepage "https://github.com/Yarden-zamir/issuegator"
  url "{{URL}}"
  sha256 "{{SHA256}}"
  license "MIT"
  head "https://github.com/Yarden-zamir/issuegator.git", branch: "main"

  depends_on "rust" => :build
  depends_on "gh"

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_predicate bin/"issuegator", :exist?
  end
end
