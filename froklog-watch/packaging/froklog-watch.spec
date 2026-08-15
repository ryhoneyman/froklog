# Built from the froklog monorepo: the client (froklog-watch/) compiles
# against the repo's own library (path = ".."), so the source unit for this
# package is the WHOLE repo tree, not just the client directory.
%global debug_package %{nil}

Name:           froklog-watch
Version:        0.1.0
Release:        1%{?gitrev:.%{gitrev}}%{?dist}
Summary:        EverQuest log watcher with DPS meter overlay, triggers and froklog streaming
# Upstream has not yet chosen a license; do not redistribute beyond your
# group until https://github.com/ryhoneyman/froklog gains a LICENSE file.
License:        Proprietary
URL:            https://github.com/ryhoneyman/froklog
Source0:        froklog-src.tar

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gcc
BuildRequires:  openssl-devel
BuildRequires:  pkgconf-pkg-config

# wgpu loads the Vulkan loader at runtime (dlopen), which rpm's automatic
# dependency scan cannot see.
Requires:       vulkan-loader
# Voice alerts: piper is not packaged in Fedora (drop a voice in
# ~/.local/share/piper); speech-dispatcher is the zero-setup fallback.
Recommends:     speech-dispatcher
# paplay, for trigger sounds and piper playback.
Recommends:     pulseaudio-utils

%description
Native tray client for EverQuest emulator logs: watches every enabled
character's log, streams each to a froklog server, and provides an
on-screen DPS meter (Wayland layer-shell overlay with click-through,
X11 fallback), trigger message overlay, sound/voice triggers with a
speaking queue, per-fight local history, and local-only mode that
needs no server at all.

%prep
%setup -q -c

%build
cd froklog-watch
cargo build --release --locked

%install
cd froklog-watch
install -Dm755 target/release/froklog-watch %{buildroot}%{_bindir}/froklog-watch
install -Dm644 froklog-watch.desktop %{buildroot}%{_datadir}/applications/froklog-watch.desktop
for size in 16 32 48 64 128 256; do
    install -Dm644 assets/icons/froklog-watch-${size}.png \
        %{buildroot}%{_datadir}/icons/hicolor/${size}x${size}/apps/froklog-watch.png
done

%files
%{_bindir}/froklog-watch
%{_datadir}/applications/froklog-watch.desktop
%{_datadir}/icons/hicolor/*/apps/froklog-watch.png

%changelog
* Wed Aug 12 2026 jadrian <jadrian2006@gmail.com> - 0.1.0-1
- Initial package: meter + message overlays, triggers, voice queue,
  fight memory, local-only mode.
