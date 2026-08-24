class TuiCalendar < Formula
  desc "Keyboard-first Apple Calendar client for the terminal"
  homepage "https://github.com/dima/apple-tui-calendar"
  url "https://github.com/dima/apple-tui-calendar/archive/refs/tags/v1.0.0.tar.gz"
  version "1.0.0"
  # PLACEHOLDER: replace with `shasum -a 256 v1.0.0.tar.gz` after publishing
  # the v1.0.0 GitHub tag. Do not release or install this formula until then.
  sha256 "0e0236393c7f41cd667c4e63fa6003f04e8fd959544315737b267ab61e900ee3"
  license "MIT"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
    cd "macos-calendar-service" do
      system "swift", "build", "-c", "release", "--disable-sandbox"
    end
    (libexec/"tui-calendar").install "macos-calendar-service/.build/release/tui-calendar-service"
  end

  def caveats
    <<~EOS
      On first launch, macOS asks for Calendar access. If access was denied,
      enable Terminal Calendar in System Settings > Privacy & Security > Calendars.
    EOS
  end

  test do
    assert_match(/^tui-calendar 1\.0\.0$/, shell_output("#{bin}/tui-calendar --version").strip)
    helper = libexec/"tui-calendar/tui-calendar-service"
    assert_predicate helper, :executable?
    input = %Q({"protocol":2,"id":1,"method":"authorizationStatus","params":{}}\n)
    assert_match(/"protocol":2,"id":1,/, pipe_output(helper, input))
  end
end
