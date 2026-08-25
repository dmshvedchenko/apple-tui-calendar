class TuiCalendar < Formula
  desc "Keyboard-first Apple Calendar client for the terminal"
  homepage "https://github.com/dmshvedchenko/apple-tui-calendar"
  url "https://github.com/dmshvedchenko/apple-tui-calendar/archive/refs/tags/v1.0.2.tar.gz"
  # PLACEHOLDER: replace after the v1.0.2 source archive is published; do not
  # publish this tap formula with the all-zero placeholder.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
    cd "macos-calendar-service" do
      system "swift", "build", "-c", "release", "--disable-sandbox"
    end
    package = libexec/"tui-calendar"
    package.install bin/"tui-calendar"
    package.install "macos-calendar-service/.build/release/tui-calendar-service"
    (bin/"tui-calendar").write_env_script package/"tui-calendar",
      TUI_CALENDAR_SERVICE: package/"tui-calendar-service"
  end

  def caveats
    <<~EOS
      On first launch, macOS asks for Calendar access. If access was denied,
      enable Terminal Calendar in System Settings > Privacy & Security > Calendars.
    EOS
  end

  test do
    assert_match(/^tui-calendar 1\.0\.2$/, shell_output("#{bin}/tui-calendar --version").strip)
    helper = libexec/"tui-calendar/tui-calendar-service"
    assert_predicate helper, :executable?
    assert_match helper.to_s, (bin/"tui-calendar").read
    input = %Q({"protocol":2,"id":1,"method":"authorizationStatus","params":{}}\n)
    output = pipe_output(helper, input)
    assert_match(/"protocol":2/, output)
    assert_match(/"id":1/, output)
  end
end
