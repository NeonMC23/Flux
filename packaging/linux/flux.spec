Name:           flux
Version:        %{flux_version}
Release:        1%{?dist}
Summary:        Local-first native music player
License:        MIT
URL:            https://github.com/NeonMC23/Flux
Requires:       alsa-lib
Requires:       fontconfig
Requires:       libX11
Requires:       libxkbcommon
Requires:       libwayland-client

%description
Flux is a native desktop music player for local audio files. It manages a
self-contained canonical library with stable artist, album, track and queue
identities, managed artwork and lyrics, playlists, ratings and playback
history. Import review is offline and never reads or rewrites audio tags.

%prep

%build

%install
rm -rf %{buildroot}
mkdir -p "%{buildroot}%{_bindir}" "%{buildroot}%{_datadir}"
install -m755 "%{flux_stage}/bin/flux" "%{buildroot}%{_bindir}/flux"
install -m755 "%{flux_stage}/bin/flux-tui" "%{buildroot}%{_bindir}/flux-tui"
cp -a "%{flux_stage}/share/." "%{buildroot}%{_datadir}/"

%files
%{_bindir}/flux
%{_bindir}/flux-tui
%{_datadir}/applications/io.github.neonmc23.flux.desktop
%{_datadir}/metainfo/io.github.neonmc23.flux.metainfo.xml
%{_datadir}/icons/hicolor/*/apps/io.github.neonmc23.flux.png
%{_datadir}/doc/flux/README.md
%{_datadir}/doc/flux/LICENSE
%{_datadir}/doc/flux/THIRD_PARTY_NOTICES.md
%{_datadir}/doc/flux/README-linux.txt
%{_datadir}/doc/flux/licenses/*

%changelog
* Sat Nov 21 2026 NeonMC23 <piotr.souchon23@gmail.com> - %{flux_version}-1
- Release candidate build.
