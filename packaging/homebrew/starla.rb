cask "starla" do
  version :latest

  on_arm do
    url "https://github.com/ananthb/starla/releases/latest/download/starla-macos-arm64.dmg"
  end
  on_intel do
    url "https://github.com/ananthb/starla/releases/latest/download/starla-macos-amd64.dmg"
  end

  name "Starla"
  desc "RIPE Atlas software probe with menu-bar tray"
  homepage "https://github.com/ananthb/starla"

  app "Starla Tray.app"
  binary "#{appdir}/Starla Tray.app/Contents/MacOS/starla"

  plist_path = "~/Library/LaunchAgents/com.ananthb.starla.plist"

  postflight do
    require "fileutils"

    home = Dir.home
    FileUtils.mkdir_p(File.expand_path("~/.config/starla"))
    FileUtils.mkdir_p(File.expand_path("~/Library/Logs"))
    FileUtils.mkdir_p(File.expand_path("~/Library/LaunchAgents"))

    config_file = File.expand_path("~/.config/starla/config.toml")
    unless File.exist?(config_file)
      File.write(config_file, <<~TOML)
        [controller]
        keepalive_interval = 60
        registration_servers = ["reg03.atlas.ripe.net:443", "reg04.atlas.ripe.net:443"]
        ssh_timeout = 30

        [probe]
        log_level = "info"

        [storage]
        max_queue_size = 10000
        retention_days = 30
      TOML
    end

    template = File.read("#{staged_path}/com.ananthb.starla.plist")
    File.write(File.expand_path(plist_path), template.gsub("HOME_DIR", home))

    system_command "/bin/launchctl",
                   args: ["unload", File.expand_path(plist_path)],
                   sudo: false,
                   must_succeed: false
    system_command "/bin/launchctl",
                   args: ["load", File.expand_path(plist_path)],
                   sudo: false,
                   must_succeed: false
  end

  uninstall_postflight do
    expanded = File.expand_path(plist_path)
    system_command "/bin/launchctl",
                   args: ["unload", expanded],
                   sudo: false,
                   must_succeed: false
    File.delete(expanded) if File.exist?(expanded)
  end

  zap trash: [
    "~/Library/LaunchAgents/com.ananthb.starla.plist",
    "~/Library/Logs/starla.log",
    "~/.config/starla",
    "~/.local/state/starla",
  ]

  caveats <<~EOS
    Starla installs a LaunchAgent at:
      ~/Library/LaunchAgents/com.ananthb.starla.plist

    Config:   ~/.config/starla/config.toml
    State:    ~/.local/state/starla/          (probe key, known_hosts)
    Logs:     ~/Library/Logs/starla.log

    Register your probe key at:
      https://atlas.ripe.net/apply/swprobe/
    The public key lives at:
      ~/.local/state/starla/probe_key.pub

    The menu-bar tray launches when you open "Starla Tray" from Applications.
  EOS
end
